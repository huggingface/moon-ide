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

## What belongs in a spec (and what doesn't)

A spec is the contract a reader needs before touching an area — not a tour of the code. Keep:

- Wire shapes, schemas, on-disk formats, command/event tables.
- Invariants and behavior the user can observe.
- The **why** behind non-obvious decisions, including rejected alternatives ("we tried 200, it was slow") — this is the part code can't carry.

Leave out:

- Implementation narration: lock orders, function-by-function walkthroughs, internal flag names, event choreography. That lives in code comments, commit messages, and test plans.
- Anything that would have to change after a behavior-preserving refactor.
- Long inline detail when a link to the file or test plan does the job.

When a section keeps growing past what a new contributor needs to _use or change_ the area safely, that's the signal to condense it and push detail down into code/test plans.

## Status legend

When a spec describes something not fully built, mark sections with one of:

- `STATUS: implemented` — code exists and matches this doc
- `STATUS: partial` — some pieces exist; gaps documented inline
- `STATUS: planned` — design only; no code yet

If the gap is too messy to caveat inline, write `STATUS: planned` and split a "Today" section that describes what actually exists.
