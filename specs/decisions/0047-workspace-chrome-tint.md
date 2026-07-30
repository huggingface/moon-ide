# ADR 0047: Per-workspace colour scheme

## Context

With process-per-workspace (ADR 0014) a user routinely has several
`moon-ide` windows open at once, and they all look identical: same
dark-blue palette regardless of which workspace owns the window. The
only per-workspace identity signal was the window icon's badge
colour (`WorkspaceMeta.color`, falling back to a hash-derived hue),
which is invisible once you're inside the window. The request: each
workspace should have its own colour scheme, in both light and dark
mode.

## Decision

One colour per workspace, reused from the existing badge colour; no
new setting. At hydrate, JS derives a full token set from that
colour's hue and writes every `--m-*` custom property onto `:root`
— twice (dark set to the plain names, light set to `*-light`
twins), so flipping `.light` stays a pure CSS change with no JS
round-trip.

The derivation is engineered per palette, not sampled:

- **Surfaces** (`--m-bg*`, `--m-border*`): the workspace hue at very
  low saturation (1-3% dark; the light palette keeps surfaces
  neutral and shows the hue in borders/overlays instead), so the
  whole window reads as "yours" without breaking contrast.
- **Text** (`--m-fg*`): neutral greys — readability over identity.
- **Accent** (`--m-accent*`): the workspace hue at full voice,
  saturation/lightness pinned per palette to a ≥4.5:1 band. Because
  `--m-accent` itself is generated, every existing accent consumer
  (focus rings, links, active states, the agent glyphs in the coder
  UI) follows the workspace with zero component changes.
- **Warning** (`--m-warning`): re-pinned to its amber ramp unless
  the workspace hue sits near it, in which case warning pushes
  toward orange so identity and severity stay distinguishable.
- `--m-ws-accent` stays as an alias of `--m-accent` for surfaces
  whose job is identity (folder bar, active tab, status-bar
  stripe), keeping a future decoupling a one-line edit.

Deliberately **not** generated: `--m-syntax-*` (code readability is
a stable palette, not identity) and ANSI terminal colours (program
output owns those; xterm's own link/highlight blue follows
`--m-accent`).

`AppInfo` grew `workspace_color` so the scheme is known at hydrate
without a second IPC; `workspace_set_color` re-applies it live when
the recoloured workspace is the one the calling process owns.

## Rejected alternatives

- **Tint surfaces at high saturation.** Reads as "a different app",
  not "my yellow workspace", and fights the red/green git and
  diagnostics signals.
- **A second "theme colour" setting.** The badge colour the user
  already picked _is_ the identity colour; a second picker is the
  "configure later" the scope rules tell us to avoid.
- **Per-workspace syntax highlighting.** Code is where muscle
  memory and a stable palette matter most; the `--m-syntax-*`
  family stays palette-owned. Revisitable independently since it's
  already a separate token family.
- **CSS `color-mix`.** WebKitGTK's version is pinned and `color-mix`
  is young; emitting concrete `hsl()` strings from JS works on the
  oldest supported syntax.

## Consequences

Unparseable or absent colours silently resolve to the hash hue,
same as the window icon — a corrupted catalog entry never produces
a broken scheme. Achromatic picks (greys) still derive a hue of 0,
so their accents go red; that reads as "red-ish workspace" rather
than "broken", which is acceptable. A user who wants the neutral
default can reset the colour to get the deterministic hash hue
back.
