# Specs

Living design docs for moon-ide. Read these before changing the system. Update them when the underlying code changes.

## Index

- [architecture.md](architecture.md) — high-level architecture, the host/agent split, the `WorkspaceHost` invariant
- [protocol.md](protocol.md) — JSON-RPC schema between UI, local core, and remote agent
- [devcontainers.md](devcontainers.md) — how containerized workspaces work end-to-end
- [frontend.md](frontend.md) — Svelte UI structure, state model, editor wiring
- [roadmap.md](roadmap.md) — phased plan and current status
- [decisions/](decisions/) — ADRs (numbered architecture decision records)

## Status legend

When a spec describes something not fully built, mark sections with one of:

- `STATUS: implemented` — code exists and matches this doc
- `STATUS: partial` — some pieces exist; gaps documented inline
- `STATUS: planned` — design only; no code yet

If the gap is too messy to caveat inline, write `STATUS: planned` and split a "Today" section that describes what actually exists.
