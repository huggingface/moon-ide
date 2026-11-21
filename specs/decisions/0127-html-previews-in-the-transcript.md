# ADR 0127: The agent can draw HTML in the transcript

## Context

Proposals (a layout, a before/after, a comparison) came back as ASCII
diagrams. The transcript already renders markdown and images; a page
of static HTML and SVG says these things better.

## Decision

- **A fence tagged `html preview`** in an agent's reply is drawn as a
  frame, with its source folded under it. Plain `html` fences stay
  code: agents quote HTML far more often than they draw it. The
  user's own messages never draw. A fence still streaming is code, so
  the frame isn't rebuilt on every delta.
- **No script runs in it.** The frame is `sandbox="allow-same-origin"`
  without `allow-scripts`: nothing inside can act, so sharing the
  origin only lets the IDE read the frame to size it and catch its
  link clicks. No forms, popups or navigation either.
- **Nothing loads.** A policy in the frame's document allows inline
  styles and `data:` images and fonts, and nothing else, so opening a
  transcript can't be observed from outside.
- **Links** never load in the frame (they target a new window, which
  the sandbox refuses); a click goes through the transcript's own
  link handling.
- The IDE's theme variables are copied into the frame so a mockup can
  look native. The system prompt tells the agent the fence, the
  limits and the variables.

## Rejected

- **Scripts in an isolated origin** (`allow-scripts` without
  `allow-same-origin`): interactive, but the IDE can't size the frame
  without a message protocol, and every page could make requests
  unless the policy is also airtight. Left for when a real need
  comes.
- **Mermaid**: flowcharts only, and a large dependency. Inline SVG
  covers it.
- **A tool that opens a browser tab**: heavier, and not part of the
  transcript.

## Not covered

- The PR tab's agent replies (GitHub-flavoured renderer): code there.
- Theme changes after a frame is drawn; the app's UI font inside it.
