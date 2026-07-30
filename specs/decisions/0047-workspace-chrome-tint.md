# ADR 0047: Per-workspace chrome tint

## Context

With process-per-workspace (ADR 0014) a user routinely has several
`moon-ide` windows open at once. The only identity signal today is
the window icon's badge colour (`WorkspaceMeta.color`, falling back
to a hash-derived hue) — invisible once you're inside the window.
The question came up whether the whole colour scheme, including
light/dark variants, could follow the workspace.

## Decision

One colour per workspace, reused from the existing badge colour; no
new setting. JS derives two `--m-ws-accent*` / `--m-ws-accent-soft*`
values from it (one per resolved palette: saturation and lightness
pinned per palette, only the hue survives) and writes all four onto
`:root` at hydrate. `:root.light` swaps which pair is live, so a
theme flip stays a pure CSS change with no JS round-trip. The vars
feed **identity surfaces only**: the active folder bar, the focused
editor tab's underline, and a 2px stripe atop the status bar.

Backgrounds, text, borders, `--m-accent`, and all `--m-syntax-*`
tokens keep their neutral palette values in both modes.

`AppInfo` grew `workspace_color` so the tint is known at hydrate
without a second IPC; `workspace_set_color` re-applies it live when
the recoloured workspace is the one the calling process owns.

## Rejected alternatives

- **Tint the whole palette from the workspace hue.** Breaks the
  danger/warning/success semantics and contrast guarantees; two
  windows would look like two different apps rather than two
  instances of one app.
- **A second user-facing "theme colour" setting.** The badge colour
  the user already picked _is_ the identity colour; a second picker
  is exactly the "configure later" the scope rules tell us to avoid.
  If a concrete need for decoupling shows up, it can be added then.
- **Per-workspace syntax highlighting.** Code readability is the one
  place where muscle memory and a stable palette matter most; the
  `--m-syntax-*` family stays palette-owned. Can be revisited
  independently since it's already a separate token family.
- **CSS `color-mix` for the soft variant.** WebKitGTK's version is
  pinned and `color-mix` is young; emitting the alpha variants from
  JS keeps us on the oldest supported syntax (`hsl()` with `/`).

## Consequences

Unparseable or absent colours silently resolve to the hash hue, same
as the window icon — a corrupted catalog entry never produces an
invisible tint. Achromatic picks (greys) still tint the chrome grey,
which reads as "neutral workspace" and is acceptable.
