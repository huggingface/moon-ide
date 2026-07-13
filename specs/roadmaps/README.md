# Roadmap details

Per-phase work breakdowns. The main [`roadmap.md`](../roadmap.md)
keeps one concise paragraph per phase and a status table; this
folder owns the sub-phase milestones, retrospective notes, and
the link map to test plans.

## When to add a file here

Add `phase-XX-<slug>.md` when a phase grows past one paragraph
in the main roadmap — typically when:

- there's more than one shippable milestone inside the phase
  (sub-phases like 11.0 / 11.1 / 11.2 …),
- the team needs an explicit acceptance bullet per milestone,
- there are open questions worth pinning to a phase rather than
  to the architectural spec.

Don't pre-create files for phases that are still single-bullet
("Phase 3 — terminal: build it"). Those entries live in
`roadmap.md` until they grow.

## What goes here vs. in `specs/<area>.md`

| `roadmaps/phase-XX.md` describes        | `specs/<area>.md` describes                                 |
| --------------------------------------- | ----------------------------------------------------------- |
| Work — verbs, milestones, acceptance    | System — nouns, contracts, invariants                       |
| "Wire X to Y, add command Z, persist W" | "X is responsible for Y. The contract is Z. Invariants: W." |
| What ships in this milestone            | How the system behaves once everything is in                |
| Open questions about scope/order        | Open questions about design                                 |
| Test-plan links                         | Failure-mode tables                                         |

If a paragraph would plausibly fit in either, it belongs in the
spec. Phase files are the changelog-of-the-future; specs are the
manual.

## Index

- [phase-01.5-editor-polish.md](phase-01.5-editor-polish.md)
- [phase-02-containers.md](phase-02-containers.md)
- [phase-02.5-multi-folder.md](phase-02.5-multi-folder.md)
- [phase-03-terminal.md](phase-03-terminal.md)
- [phase-05-git.md](phase-05-git.md)
- [phase-06-coder.md](phase-06-coder.md)
- [phase-07-multi-workspace.md](phase-07-multi-workspace.md)
- [phase-11-slack-chat.md](phase-11-slack-chat.md)
- [phase-13-mobile-companion.md](phase-13-mobile-companion.md)
- [phase-14-remote-bridge.md](phase-14-remote-bridge.md)
