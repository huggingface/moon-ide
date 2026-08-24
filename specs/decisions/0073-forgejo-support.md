# ADR 0073 — Forgejo support (fj CLI + direct REST)

## Context

The forge integration — blame / permalink / `#123` links, the
branch-switcher PR section, `pr checkout`, the SCM panel's Open PR
button, and review publishing — was GitHub-only, built on `gh`.
Team members also work on Forgejo-hosted repos (codeberg.org and
self-hosted instances) and asked for the same support there via the
`fj` CLI ([forgejo-cli](https://codeberg.org/forgejo-contrib/forgejo-cli)).

`fj` (v0.6.0) is not `gh`: it has **no machine-readable output**
(its listings are localized Fluent text), **no `api` passthrough**,
and **no review-creation command**. Only `fj pr checkout` maps 1:1.

## Decision

- **Detection** lives in `crates/moon-core/src/forge.rs`
  (`detect_forge_remote`): `github.com` → GitHub; `codeberg.org` or
  any host present in fj's `keys.json` (`hosts` map + `aliases`) →
  Forgejo. Anything else stays unsupported
  (`PrListStatus::UnsupportedRemote`, no links). An fj login is the
  explicit "this host is a Forgejo instance" signal.
- **URL shapes** switch on the forge kind: permalinks use Forgejo's
  `/src/commit/<sha>/<path>`, the create-PR URL uses the
  `compare/<default>...<branch>` page, and `#N` references link to
  `/issues/N` (both forges redirect that to the PR page).
- **`fj pr checkout <n>`** backs the branch-switcher PR checkout,
  mirroring `gh pr checkout`.
- **Everything else goes straight to the instance's `/api/v1`**
  (PR list, existing-PR URL, review POST), authenticated with the
  token fj already stores in `keys.json`. This deliberately relaxes
  ADR 0027's "shell out to gh, never a raw token or `reqwest`" —
  for Forgejo only, because fj offers no CLI path for this data.
  The spirit is kept: fj owns the credential (we never prompt for,
  store, or log a token), and a 401 triggers one best-effort
  `fj whoami --host <host>` run so fj itself refreshes an expired
  OAuth token before the single retry. Reads work anonymously on
  public repos.
- **Semantics gaps, accepted:** Forgejo review comments are
  single-line (`new_position` / `old_position`), so a multi-line
  draft anchors on its last line; the `Participating` PR scope
  filters client-side to author / assignee / requested reviewer
  (no `involves:` equivalent, mentions and comments don't count).
- **Containers:** the host's fj data dir bind-mounts read-only
  (see [containers.md](../containers.md)); moon-base builds fj
  0.6.0 from source in a builder stage because upstream binaries
  need glibc ≥ 2.38 and the image is bookworm (2.36).

## Rejected alternatives

- **Parsing fj's human output** for the PR list — localized and
  unstable by design; a locale change would silently break it.
- **Upstreaming JSON output / review commands to fj first** — the
  right long-term fix, but it gates the feature on an external
  release cycle; the REST API is stable and versioned.
- **Links-only support** (no PR list / publish) — asked and
  answered: the team wants parity.
- **Probing unknown hosts** (`GET /api/v1/version`) to auto-detect
  self-hosted Forgejo — a network probe per repo open, and it
  guesses; the keys.json signal is explicit and free.
