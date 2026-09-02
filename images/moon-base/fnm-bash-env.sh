# Sourced by every non-interactive bash (`bash -c`) via `$BASH_ENV`
# — see the Dockerfile's fnm block. Interactive shells get the same
# setup from `~/.bashrc` instead; keep the two in sync.

# Set up fnm per-shell: the eval creates the `fnm_multishells` dir
# and prepends its bin to PATH so the switched-to Node (and its
# corepack shims) resolve for the rest of the command.
eval "$(fnm env --version-file-strategy=recursive --shell bash)"

# Switch to the nearest `.nvmrc` / `.node-version` from $PWD down.
# No version file anywhere up the tree: falls back to the `default`
# alias silently. Missing version: installs it rather than failing.
# `FNM_LOGLEVEL=quiet` keeps the "Using Node vX" info line out of
# captured stderr — this file runs for every non-interactive bash,
# including the coder's `bash` tool results.
FNM_LOGLEVEL=quiet fnm use --install-if-missing --silent-if-unchanged
