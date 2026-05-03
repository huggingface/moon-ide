# Test plans

Written test plans for non-trivial commits. Drafted **before** the commit, refined while the human is testing, and committed alongside the code change. Numbered like ADRs (`NNNN-short-slug.md`); numbers never reused.

## Why we keep them

- They survive the conversation that produced them. Once a chat is gone, the why-and-how-to-test goes with it. A file in the repo doesn't.
- They give a future agent something to reference when fixing a regression in the same area: "this is what was supposed to keep working".
- They make `git blame` a useful starting point — find the file, find the commit, find the test plan.
- They force us to write down the steps we'd otherwise rattle off in a chat reply, so the human reviewer doesn't have to reconstruct them.

## When to write one

**Yes**:

- Phase deliverables (one per phase, at minimum).
- Any commit that adds, changes, or removes an IPC method, a Tauri command, or a `WorkspaceHost` trait method.
- New UI surfaces (new component a user will interact with, new keybinding, new menu).
- Bug fixes that cross more than one layer (UI ↔ core ↔ host).
- Anything that introduces a new dependency or tool.

**No**:

- Pure formatting / lint-only changes.
- Single-file refactors with no behavioral change.
- Comment / doc-only changes.
- Rename-only changes.

When in doubt: write one. They are cheap.

## Format

Each file is a single markdown document. Use `0000-template.md` as the starting point. The required headers are:

- **Date** the plan was written. Commit-level identification is left to `git blame` / `git log` — the file's history in the repo is the source of truth for "which commit introduced or last touched this plan", so duplicating that in the front-matter just creates a stale field nobody fills in.
- **What shipped** — a handful of **high-level** bullets (aim for ≤ 6, one line each) that describe the user-visible outcome and the main architectural moves. This is not a changelog and not a file-by-file diff — `git log -p` already does that better. If you find yourself listing every helper function you touched, stop and compress. A reader should understand _what the commit does_ without opening any code.
- **How to test** — numbered steps a human can run end-to-end. Include exact paths, commands, and expected outputs where they matter. This section is allowed (and expected) to be longer than "What shipped"; it's the section that earns the file its keep.
- **What must keep working** — regression checks. The reason this file is in the repo and not a chat.
- **Known limitations** — anything we deliberately didn't do.
- **Related** — links to specs, ADRs, prior test plans.

## Numbering and ordering

- Increment the leading number by one per plan; never reuse.
- Plans are not superseded — each one is a snapshot of "what was true at commit X". If a later commit invalidates a step, write a new plan; don't edit the old one.
- The history is append-only on purpose. The set of plans is the testable history of the project.
