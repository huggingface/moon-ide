# Test plan 0039: SSH agent forwarding into the dev container

- **Date**: 2026-05-05
- **Phase**: Phase 2 polish (workspace shell)

## What shipped

- The generated workspace `compose.yaml` now bind-mounts the
  host's SSH agent socket into the `dev` service and sets
  `SSH_AUTH_SOCK` inside the container, so `git fetch`,
  `git push`, `gh`, and any other ssh-using tool the user runs
  in a container terminal can reach the host's keys without
  copying private material.
- Host-side socket resolution is platform-aware: macOS uses
  Docker Desktop's magic `/run/host-services/ssh-auth.sock`
  unconditionally; Linux reads `$SSH_AUTH_SOCK` from the IDE's
  env and skips the forward (with a `tracing::warn!`) if it's
  unset or the socket doesn't exist. Container-side path is
  `/run/host-services/ssh-auth.sock` on both platforms.
- `moon-base` now installs `openssh-client` (so `ssh` is on
  `PATH`) and pre-seeds `/etc/ssh/ssh_known_hosts` with
  `github.com` and `gitlab.com` keys via `ssh-keyscan` at image
  build time, so the first SSH-backed `git fetch` doesn't
  prompt on host-key acceptance under non-interactive
  `docker exec`.
- Spec coverage in `specs/containers.md` § "SSH agent
  forwarding"; the `Workspace::render_compose` /
  `Workspace::write_state` paths consult
  `detect_ssh_agent_forward()` on every call, so the
  "Inspect compose.yaml" preview matches what would land on
  disk.

## How to test

Prerequisites:

- `bun install`
- Tauri dev deps per `README.md`.
- Docker Engine + Compose v2 on the host.
- A locally rebuilt `moon-base:dev` image after this commit:
  `docker build -t moon-base:dev images/moon-base/`.
- A running ssh agent with at least one key loaded:
  `ssh-add -l` should show a key. On Linux launches from a
  graphical session this is usually `gnome-keyring` or a
  user-level `ssh-agent`. On macOS, Docker Desktop's magic
  socket is always available as long as Docker Desktop is
  running.
- A real git remote you can fetch from over SSH (e.g. this
  repo's own `git@github.com:huggingface/moon-ide.git`).

### A. Forward landed in the rendered compose

1. Wipe persisted state for a clean baseline:
   - `rm -f ~/.config/moon-ide/state.json`
   - `rm -rf ~/.local/share/moon-ide/workspaces/`
2. `bun run tauri dev`. Open `~/code/moon-ide` (or any folder
   with a real git remote configured for SSH).
3. Container popover → "Inspect compose.yaml". Expected: the
   preview contains a `dev.environment.SSH_AUTH_SOCK:
/run/host-services/ssh-auth.sock` entry and a volume line
   binding the host's agent socket onto that path.
   - **Linux**: the host side is whatever `echo $SSH_AUTH_SOCK`
     prints in the same shell that launched moon-ide.
   - **macOS**: the host side is `/run/host-services/ssh-auth.sock`
     verbatim.
4. Click "Set up". Status pip cycles through `setting up…` →
   `running`.
5. Confirm the file matches the preview:
   `cat ~/.local/share/moon-ide/workspaces/default/compose.yaml`
   shows the same volume + environment block.

### B. Agent reaches the container

1. With the dev container running from §A:
   `docker compose -p moon-ws-default exec dev bash -c 'ssh-add -l'`.
   Expected: lists the same keys `ssh-add -l` showed on the
   host.
2. `docker compose -p moon-ws-default exec dev bash -c \
'cd /workspace/moon-ide && git fetch'`. Expected: completes
   without a host-key prompt and without an "agent has no
   identities" error.
3. `docker compose -p moon-ws-default exec dev bash -c \
'ssh -T git@github.com'`. Expected: `Hi <user>! You've
successfully authenticated, but GitHub does not provide
shell access.` (exit code 1 is fine — that's GitHub's
   convention.)

### C. Linux fallback when no agent is running

1. Quit moon-ide.
2. From a shell with **no** `SSH_AUTH_SOCK` set:
   `unset SSH_AUTH_SOCK && bun run tauri dev`.
3. Open the same workspace, "Inspect compose.yaml". Expected:
   no `SSH_AUTH_SOCK` env entry, no agent-socket volume line.
   The `dev` service should still render with the bound-folder
   mounts.
4. Click "Set up". Status pip cycles to `running`.
5. `docker compose -p moon-ws-default exec dev bash -c \
'echo $SSH_AUTH_SOCK'`. Expected: empty.
6. Look at the IDE process's stderr (the terminal that ran
   `bun run tauri dev`). With `unset SSH_AUTH_SOCK` no warning
   is emitted — the env-missing path takes the `debug!`
   codepath, which is silent at the default `info` log level.
   To verify the warn path explicitly, point `SSH_AUTH_SOCK`
   at a non-existent file:
   `SSH_AUTH_SOCK=/tmp/no-such-socket bun run tauri dev` and
   `setup`/`rebuild`. Expected: one
   `WARN ... skipping ssh agent forwarding` line.

### D. Pre-seeded host keys keep `docker exec` non-interactive

1. From a fresh container (rebuilt off this commit's
   `moon-base`):
   `docker compose -p moon-ws-default exec dev bash -c \
'ssh -o BatchMode=yes -T git@github.com; echo exit=$?'`.
   Expected: exit 1 (GitHub's "no shell access" message
   landed) — **not** exit 255 with `Host key verification
failed`. `BatchMode=yes` rejects interactive prompts, so
   passing this step proves the key was pre-trusted.
2. `docker compose -p moon-ws-default exec dev cat \
/etc/ssh/ssh_known_hosts | wc -l`. Expected: at least 4
   lines (rsa + ecdsa + ed25519 for both github.com and
   gitlab.com — exact count varies as providers add/remove
   key types).

### E. Rebuild reflects the IDE's current environment

1. Quit moon-ide. `unset SSH_AUTH_SOCK`. Launch moon-ide
   without an agent reachable. Set up the container; verify §C
   shape (no forward).
2. Container popover → "Rebuild". Expected: still no forward
   — the rebuild re-reads the IDE process's own env, which
   wasn't updated by `unset` happening before launch.
3. Now quit the IDE, start an agent in the launching shell
   (`eval "$(ssh-agent -s)"; ssh-add ~/.ssh/id_ed25519`), and
   relaunch moon-ide from that shell. "Inspect compose.yaml"
   should now contain the env entry + volume line; "Rebuild"
   will pick it up on the next dev container recreate.

   **Limitation made explicit**: starting an agent _after_
   the IDE has launched does not flow into the IDE process's
   environment — relaunching is the only way to pick it up
   short of a future "Forward host SSH agent" toggle that
   queries the user's agent path on demand.

## What must keep working

- Container `setup` / `pause` / `resume` / `rebuild` /
  `teardown` lifecycle (test plans 0011, 0012, 0015).
- Folder add / remove regenerating `compose.yaml` (test plan
  0011 §B / §D).
- The "Inspect compose.yaml" affordance reflects what `setup`
  would write (test plan 0011 §G), now including the agent
  block.
- LSP container fallback (test plan 0031) — `docker exec`
  invocations into the dev container still resolve to the
  same shell environment, which now has `SSH_AUTH_SOCK`
  exported when forwarding is on.

## Known limitations

- **`SSH_AUTH_SOCK` is sampled once per compose write.** A
  user who starts an agent after launching moon-ide has to
  relaunch under an agent-bearing shell (or set
  `SSH_AUTH_SOCK` and trigger a rebuild). The IDE doesn't
  watch for agent-socket appearance.
- **`~/.ssh/config` is not forwarded.** Anyone whose git
  remotes rely on a custom `Host` alias or an `IdentityFile`
  pointing at a non-default key needs to either rewrite the
  remote URL or layer their own `~/.ssh/config` mount in a
  team Dockerfile.
- **GPG agent (signed commits) is not forwarded.** Easy to
  layer on per-team Dockerfile if it shows up.
- **Pre-seeded known_hosts can drift.** If GitHub or GitLab
  rotate keys between image rebuilds, the next
  `ssh git@github.com` from inside the container falls back
  to the prompt-based accept flow (which fails under
  non-interactive `docker exec`). The fix is a `moon-base`
  rebuild; until that ships, `ssh -o
StrictHostKeyChecking=accept-new` is the manual escape.

## Related

- Spec: [`containers.md` § SSH agent forwarding](../containers.md#ssh-agent-forwarding)
- ADRs: [`0007-compose-and-moon-base.md`](../decisions/0007-compose-and-moon-base.md),
  [`0008-host-shared-daemon.md`](../decisions/0008-host-shared-daemon.md)
- Prior test plans: 0011 (state dir + multi-folder mounts),
  0012 (workspace shell vs project services), 0031 (Rust LSP
  in container).
