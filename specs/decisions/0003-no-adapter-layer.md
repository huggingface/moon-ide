# ADR 0003 — No adapter layer for assembled components

Date: 2026-04-26
Status: accepted

## Context

The original plan proposed a `packages/adapters/` package wrapping every external component (Pierre Trees, Pierre Diffs, xterm, CodeMirror) behind a project-defined interface, so that any of them could be swapped without touching consumers.

## Decision

Don't do that. Use the libraries directly.

## Why

- Adapter layers calcify quickly. The lowest-common-denominator API loses features the underlying library has.
- We are a small team. Refactoring with AI assistance is cheap; designing speculative interfaces is not.
- Each "potentially swappable" component is in fact used in exactly one place (FileTree.svelte, Editor.svelte, etc.). That single component IS the adapter — adding another layer doubles the indirection.

## Consequences

- If we ever want to swap Pierre Trees for something else, the change is scoped to one Svelte component.
- If a library's API forces a non-trivial leak (e.g. its types appear all over the codebase), that's a smell. Contain it then; not preemptively.
- Code stays flatter and easier to read.

## Supersedes

This supersedes the "thin wrappers in `packages/adapters/`" idea from the initial plan.
