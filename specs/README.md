# Specs

Living design docs for moon-ide. Read these before changing the system. Update them when the underlying code changes.

## Index

- [architecture.md](architecture.md) — high-level architecture, the host/agent split, the `WorkspaceHost` invariant
- [protocol.md](protocol.md) — JSON-RPC schema between UI, local core, and remote agent
- [containers.md](containers.md) — how containerised workspaces work end-to-end (Phase 2)
- [editorconfig.md](editorconfig.md) — `.editorconfig` end-to-end model
- [slack-chat.md](slack-chat.md) — Slack chat panel architecture
- [companion.md](companion.md) — mobile companion app via the `moon-bridge` daemon (planned)
- [frontend.md](frontend.md) — Svelte UI structure, state model, editor wiring
- [roadmap.md](roadmap.md) — phased plan and current status (one paragraph per phase)
- [roadmaps/](roadmaps/) — per-phase work breakdowns when a phase grows past one paragraph
- [decisions/](decisions/) — ADRs (numbered architecture decision records)
- [test-plans/](test-plans/) — written test plans for major features and phase deliverables (most commits don't get one)

Phase files in `roadmaps/` are about **work** (verbs, milestones, acceptance bullets); area specs in `specs/<area>.md` are about the **system** (nouns, contracts, invariants). If a paragraph could plausibly fit in either, it belongs in the spec.

## Status legend

When a spec describes something not fully built, mark sections with one of:

- `STATUS: implemented` — code exists and matches this doc
- `STATUS: partial` — some pieces exist; gaps documented inline
- `STATUS: planned` — design only; no code yet

If the gap is too messy to caveat inline, write `STATUS: planned` and split a "Today" section that describes what actually exists.
