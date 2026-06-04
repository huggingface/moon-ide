# ADR 0025 — Coder file tools reach arbitrary paths, container-aware

Date: 2026-06-12
Status: accepted; amends [`specs/coder.md`](../coder.md) (the path
resolution / cross-folder routing section) and evolves the file-tool
note in [ADR 0022](0022-coder-host-mode-override.md).

## Context

The coder's four filesystem tools — `read_file`, `list_dir`,
`write_file`, `edit_file` — dispatch through the active folder's
[`WorkspaceHost`](../architecture.md#workspacehost-phase-2). Today
that host is always `LocalHost`, whose `resolve` rejects any path
that doesn't sit under the bound folder's root:

```rust
if !canonical.starts_with(&self.root) {
    return Err(MoonError::PermissionDenied(
        format!("path {canonical} escapes workspace root")));
}
```

So the agent could read and write only inside a bound folder. Real
tasks need more: inspecting `/etc/hosts`, reading a tool's config
under `$HOME`, checking a build artifact that lands outside the
project tree, writing a scratch file in `/tmp`. The agent already
does all of this through `bash` (`cat`, `ls`, here-docs), but
shelling out for a read it has a first-class tool for is clumsy,
loses the line-numbered framing, and doesn't share the
`edit_file` fuzzy-match machinery.

A second wrinkle: where do "arbitrary" paths live? `bash` routes
into the workspace shell container via `docker exec` when that
container is `Running` (ADR 0022), and to the host otherwise. The
file tools, by contrast, have always been host-direct — they read
the bind-mounted source on the host disk, never crossing into the
container (ADR 0022's "file tools are already host-direct through
the container bind mount" note). For in-workspace paths that's
correct and stays unchanged: the bind mount means the same bytes
are visible host-side and container-side, and going host-direct
keeps format-on-save, editorconfig, and the future in-container
`RemoteHost` all working through one code path.

But for paths _outside_ the bind mount, host-direct and container
diverge. `/etc/hosts` on the host is a different file from
`/etc/hosts` in the container. If the agent `ls`'d `/etc` via
`bash` (which ran in the container) and then `read_file`'d
`/etc/hosts` (host-direct), it would get two different
filesystems — confusing and wrong.

## Decision

Lift the workspace-root gate for the four file tools, and make
out-of-workspace access **container-aware**, matching where `bash`
runs.

Path resolution now returns a `ResolvedTarget`:

- **`InWorkspace { folder, relative }`** — path lands inside a
  bound folder. Unchanged: dispatch through that folder's
  `WorkspaceHost`, keeping format-on-save and the bind-mount
  host-direct path. This is the overwhelmingly common case.
- **`OutOfWorkspace { abs_path }`** — absolute path outside every
  bound folder root (and not a `/workspace/<name>` synthetic
  path). Dispatch through new container-aware primitives that
  mirror `bash`'s target: `docker exec` into the workspace shell
  container when it's `Running`, the host filesystem otherwise.
  The same `resolve_bash_target` probe (force-host override
  included) picks the target, so a file the agent `ls`'d via
  `bash` is the same file `read_file` opens.

Out-of-workspace writes skip format-on-save entirely: there's no
project to anchor a `.editorconfig` / lint-staged cascade, so the
write lands the model's exact bytes and never enters the turn-end
`FormatQueue`.

The container primitives are deliberately small and shell-free
where it matters: reads use `docker exec <name> cat -- <path>`
(direct exec — no word-splitting or glob expansion of the path),
writes use `docker exec -i <name> cp /dev/stdin <path>` with the
content piped on stdin, and `list_dir` uses one `find -maxdepth 1`
exec with a type tag per child. Only `list_dir`'s `find` needs a
shell, and its single interpolated path is single-quote-escaped.

## Why not a `ContainerHost: WorkspaceHost` impl instead?

A full in-container `WorkspaceHost` is the eventual Phase 2 story
for _in-workspace_ I/O, and it'll subsume the bind-mount
host-direct path when it lands. But arbitrary-path access is a
coder-specific capability that should **not** become a general
`WorkspaceHost` method — the UI's file tree, git integration, and
editor must stay gated to bound folders (cross-cutting invariant:
"anything that does I/O on the workspace goes through the active
`WorkspaceHost`"). Putting `read_anywhere` on the trait would
hand every consumer an ungated escape hatch. Keeping the
out-of-workspace primitives inside `moon-coder` confines the
capability to the agent, which is exactly the blast radius we
want.

## Consequences

- The agent can read/write/list any path the bash target can
  reach. This is a real expansion of what a turn can touch
  (`~/.ssh`, `~/.bashrc`, `/etc/...`). The agent runs with the
  user's trust already (it has `bash`), so this grants no new
  privilege — only a more ergonomic surface for what `bash`
  could already do. The system prompt nudges the agent to stay
  in-workspace for normal work and reach out only when the task
  needs it.
- `/workspace/<name>` whose `<name>` is **not** a bound folder
  still errors with the bound-folder list rather than silently
  falling to the out-of-workspace path. The prompt only
  advertises that synthetic form for bound folders, so an
  unbound `<name>` is a model mistake worth a precise nudge.
- In-workspace behaviour — format-on-save, editorconfig,
  `FormatQueue`, the cross-folder routing — is byte-for-byte
  unchanged. Only absolute paths outside every bound folder take
  the new branch.
- ADR 0022's "file tools are host-direct, never `docker exec`"
  note now holds **only for in-workspace paths**. Out-of-workspace
  file ops cross into `docker exec` exactly like `bash`.

## No premature migration

Per the repo's "no premature migrations" rule, there's no compat
shim for the old gate: the resolver simply returns the new
`ResolvedTarget` and the tools branch on it. The previous
`resolve_workspace_path` survives only as a `#[cfg(test)]`
flattening helper.
