# ADR 0020 — Process-wide TTL cache for `docker compose ps`

Date: 2026-05-26
Status: accepted; refines
[`specs/containers.md`](../containers.md#per-folder-compose-projects)
and the [LSP routing table](../lsp.md#container-backed-lsp).

## Context

Every LSP IPC routes through `ensure_broker` in
`src-tauri/src/commands/lsp.rs`, which calls `resolve_target` to
pick host-vs-container routing for the spawner. `resolve_target`
calls `Workspace::status()`, which shells out to
`docker compose -f <state-dir>/compose.yaml -p moon-ws-<id> ps --all --format json`.
No caching — each call re-invokes `docker compose ps`.

In normal editing, that fans out badly. A single keystroke that
triggers tsgo's completion hot path (typing `${` inside a TS
template string is the case the bug landed on) produces, in one
burst:

- `lsp_update` (didChange) for the document
- `lsp_completion` (member-expression / template completion)
- `lsp_pull_diagnostics` follow-up
- frequently `lsp_hover` or `lsp_definition` if the cursor sits on a name

…and a moon-ide workspace typically has multiple LSPs live for
the same file (tsgo + oxlint, or tsgo + tailwind, etc.) plus
sibling folders contributing their own commands. Real logs from
the bug report show 30+ `docker compose ps` invocations within
~900 ms, each one a 50–200 ms shell-out that competes with the
daemon and blocks the IDE — the editor visibly freezes.

The same fanout potential exists for `ProjectCompose::status()`
(per-folder services), but the symptom there is muted by lower
call frequency.

We can't simply stop calling `status()`. Other callers — the
folder-bar 2 s / 15 s pollers, `container_status` on window
focus, the coder's bash routing, shutdown's snapshot pass —
genuinely need a fresh reading. The defensive shape is a cache,
not architectural surgery on every call site.

## Decision

Put a **process-wide TTL cache** in front of `Workspace::status()`
and `ProjectCompose::status()`. Lives at
`crates/moon-container/src/status_cache.rs`; everything else is
unchanged.

- **Key**: `(ProjectName, compose_file_path)`. Together they pin
  the same `-p` and `-f` flags every `docker compose` call uses,
  so equal keys see identical output.
- **TTL**: 1 s. Long enough to collapse a per-keystroke burst
  into one shell-out; short enough that an external mutation
  (`docker compose down` from a terminal, a daemon hiccup) shows
  up within the same window the folder-bar already polls at
  (2 s). Captured as a `const`; no need to make it configurable.
- **Errors are not cached**. A transient `DaemonUnreachable` /
  `ComposeFailed` should not pin the routing decision for a
  whole second after recovery; only `Ok(ContainerStatus)`
  enters the map.
- **Mutating commands invalidate**. `Workspace::{setup,
apply_bound_folders, pause, resume, rebuild, stop, teardown}`
  and the `ProjectCompose` equivalents call
  `status_cache::invalidate(...)` on success. The existing
  `snapshot_and_emit` path in `src-tauri/src/commands/container.rs`
  calls `status()` immediately after mutations, so the
  post-mutation reading lands in the cache and gets reused for
  the next TTL window.
- **No request coalescing**. Two concurrent cache misses each
  shell out, both write back, last writer wins. `ps` is
  idempotent and the duplicate is rare (the first fill ends the
  race for the rest of the TTL). A `tokio::sync::OnceCell`-style
  coalescer is reachable later if a benchmark ever justifies it.

## Consequences

**Good.**

- The 30-call-per-keystroke burst collapses to one call per
  second per `(project, compose_file)` pair, eliminating the
  visible freeze on `${` (and on every other completion-heavy
  edit shape).
- All other callers — pollers, status events, coder routing,
  shutdown snapshot — automatically benefit. Cache cost is one
  `Mutex<HashMap>` lookup; the previous cost was a process
  spawn.
- Refresh model lines up with what the folder-bar pollers
  already commit to: external changes show up within ~1–2 s,
  which is what the UI already promised.

**Bad / accepted.**

- Process-global mutable state in `moon-container`, which the
  lifecycle module's preamble previously bragged about avoiding
  ("thin shell-out, no global state"). Acceptable: the cache
  _is_ a property of "talking to `docker compose ps`", and any
  alternative (cache in `src-tauri`, or threading a cache handle
  through every call site) makes the wrong default cheap to
  forget. Documented at the top of `status_cache.rs`.
- Up to 1 s of stale `status()` is observable to external
  callers. Matters in exactly one place: a user running
  `docker compose down` from a host terminal won't see the
  status pill flip immediately. The 2 s folder-bar poll already
  has this exact property, so the UX bar moves up rather than
  down.
- Tests have to use distinct cache keys because the cache is
  process-wide and `cargo test` runs in parallel. Cheap
  workaround (one keyword per test); not worth a thread-local
  cache.

## Out of scope

This ADR does not touch `ensure_broker`'s per-IPC call pattern.
After the cache lands, `resolve_target` becomes a sub-millisecond
hashmap lookup, so the per-IPC cost is bounded. A separate
follow-up could make `ensure_broker` trust its cached
`BrokerTarget` directly and skip `resolve_target` entirely (only
re-resolving on folder switch / container lifecycle events), but
that's a structural change with its own tradeoffs and is left
for a future ADR if the bounded cost ever becomes the bottleneck.

## Alternatives considered

- **Cache the `BrokerTarget` in `ensure_broker` instead of the
  status reading itself.** Smaller change, but only fixes the
  LSP path; every other `status()` caller (pollers, coder
  routing, shutdown) would still shell out per call. The
  read-path cache fixes everything at once.
- **Inject the TTL through `WorkspaceConfig`.** Pure overhead
  until someone produces a second concrete value to inject.
  Hardcode now (per project rules), configure later if needed.
- **Coalesce in-flight calls (`OnceCell`-style).** Strictly
  better for the cold-burst worst case, but the cache fill is
  150 ms in the bad case and the duplicate call only happens at
  cache start. Not worth the complexity until measured.
