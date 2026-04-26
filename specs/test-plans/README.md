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

- **Date** and **commit / branch** the plan covers.
- **What shipped** — bullets, terse, no marketing.
- **How to test** — numbered steps a human can run end-to-end. Include exact paths, commands, and expected outputs where they matter.
- **What must keep working** — regression checks. The reason this file is in the repo and not a chat.
- **Known limitations** — anything we deliberately didn't do.
- **Related** — links to specs, ADRs, prior test plans.

## Numbering and ordering

- Increment the leading number by one per plan; never reuse.
- Plans are not superseded — each one is a snapshot of "what was true at commit X". If a later commit invalidates a step, write a new plan; don't edit the old one.
- The history is append-only on purpose. The set of plans is the testable history of the project.
