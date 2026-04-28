# 0013 — Slack reaction display (Phase 11.3.1)

Small follow-up to 11.3: render the `reactions` array Slack
already returns on every message that has any. Read-only —
tapping a chip doesn't toggle anything yet. Detailed scope lives
in `slack-chat.md` (look for "No reaction add/remove from the
panel yet.").

## What ships

- `RawMessage::reactions` parsed in `moon-slack`, mapped to a new
  `SlackReaction { name, count }` on the protocol
  (`crates/moon-protocol/src/slack.rs`). The full Slack reactor
  list is parsed but not surfaced — kept on the Rust side via
  `#[allow(dead_code)]` so the next phase that wants a "you
  reacted" highlight or hover tooltip doesn't have to re-touch
  the wire types.
- New `resolveReactionName(name)` helper in
  `src/lib/util/slackEmoji.ts` — strips `::skin-tone-N`
  modifiers, runs the existing CLDR + `SLACK_ALIASES` resolution,
  falls back to `:name:` for custom workspace emoji.
- `ChatPanel.svelte` renders chips below the message body, above
  any action buttons. Small pill styling using the existing
  `--m-bg-2` / `--m-border` tokens; no new theme entries.
- 3 Rust unit tests:
  - `reactions_are_extracted_and_zero_count_dropped`
  - `missing_reactions_field_is_empty_vec`
  - `skin_tone_modifier_passes_through_verbatim`

## Setup

1. `bun run dev`, connect Slack, open any thread that has
   reactions on it. moon-bot's status replies (`:white_check_mark:`,
   `:warning:`, `:x:`) are the canonical examples.
2. If you don't have one handy, react to one of moon-bot's
   replies from regular Slack (mobile / web / desktop), wait for
   the next poll tick (≤ 3 s for a hot thread), and the chip
   should appear.

## Scenarios

### A — Standard emoji renders as a glyph

1. React with `:thumbsup:` from regular Slack.
2. Within one cadence tick, a chip appears below the message
   body: `👍 1`.
3. Add another reactor (different account or different emoji on
   the same message): the count bumps to `2` for the same emoji,
   or a second chip appears for a new emoji.

### B — Multiple reactions on one message

1. Stack three different reactions on a single message
   (`:rocket:`, `:white_check_mark:`, `:thumbsup:`).
2. Three chips render in the order Slack returned them
   (roughly first-reacted-first), all on the same row, wrapping
   if the message is narrow.

### C — Custom workspace emoji falls back to `:name:`

1. React with a workspace-specific emoji (e.g. a Hugging Face
   logo upload). `node-emoji` won't resolve this.
   - Expected: chip renders as `:hf-logo: 1` (text, not a
     glyph). The bot intent is preserved.
2. Hover the chip: tooltip shows `:hf-logo:` (the `title`
   attribute is set on every chip regardless of resolution).

### D — Skin-tone modifier strips to base

1. React with `:+1:` and pick a skin tone from Slack's picker.
2. The chip renders the base `👍` (no skin colourisation). Count
   is correct.
   - Known limitation; lifted whenever the wider emoji story
     gets attention.

### E — Removed reactions disappear

1. With a reaction on a message, remove your reaction in regular
   Slack.
2. Wait for the next cadence tick. The chip count decrements; if
   you were the last reactor, the chip disappears entirely.

## Known limitations (deliberate)

- No add/remove from the panel — the chips are non-interactive
  in v1.
- No "you reacted" highlight — Rust parses the `users` array but
  the frontend doesn't see it yet.
- No custom workspace emoji `<img>` rendering.
- No tooltip with reactor names — that's the next chunk if
  somebody asks. The minimal cut keeps the chip noise low.
