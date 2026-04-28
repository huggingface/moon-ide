// Slack `:shortcode:` → Unicode emoji resolution.
//
// Slack accepts a slightly-extended set of shortcodes — mostly
// gemoji-era aliases that pre-date the CLDR canonicalisation — and
// renders the matching Unicode glyph (or, for workspace-uploaded
// custom emoji, an `<img>`). For the read-only chat panel we only
// resolve the standard ones; custom workspace emoji come back as
// `:shortcode:` and we leave them as text (Phase 11.4+ would fetch
// `emoji.list` and render the URL).
//
// `node-emoji` covers most of the common set under CLDR names. The
// `SLACK_ALIASES` table below patches the ones Slack still calls by
// their pre-CLDR name (`robot_face`, `tools`, `thinking_face`, …).
// Add to it as users surface gaps.

import { get } from 'node-emoji';

// Canonical-name aliases used by Slack but not by `node-emoji`. Map
// from Slack's preferred shortcode to a name `node-emoji` does know.
// Lowercase keys; `node-emoji` is also case-sensitive.
const SLACK_ALIASES: Record<string, string> = {
	robot_face: 'robot',
	tools: 'hammer_and_wrench',
	thinking_face: 'thinking',
	thumbsup: '+1',
	thumbsdown: '-1',
	hankey: 'poop',
	shit: 'poop',
	pile_of_poo: 'poop',
	face_with_rolling_eyes: 'rolling_eyes',
	upside_down_face: 'upside_down',
	face_with_thermometer: 'thermometer_face',
	zipper_mouth_face: 'zipper_mouth',
	hugging_face: 'hugging',
	man_in_business_suit_levitating: 'levitate',
	the_horns: 'sign_of_the_horns',
	love_letter: 'love_letter',
	bow: 'person_bowing',
	raising_hand: 'person_raising_hand',
	tipping_hand: 'person_tipping_hand',
	face_palm: 'facepalm',
	shrug: 'person_shrugging',
};

/**
 * Match a single Slack-style shortcode. The body matches Slack's
 * accepted character set (lowercase letters, digits, `_`, `+`, `-`).
 * Anchored loosely so a bare `:not_an_emoji:` inside prose still
 * matches and gets a chance at lookup.
 */
const SHORTCODE_RE = /:([a-z0-9_+-]+):/g;

/**
 * Replace every `:shortcode:` in `text` with the matching Unicode
 * glyph. Unknown shortcodes (custom workspace emoji, typos, or
 * niche names neither `node-emoji` nor [`SLACK_ALIASES`] cover) are
 * left untouched so the user still sees the bot's intent.
 *
 * Pure / synchronous — safe to call inside the tokenizer's render
 * pass. No DOM, no allocation when there are no shortcodes.
 */
export function emojify(text: string): string {
	if (!text.includes(':')) {
		return text;
	}
	return text.replaceAll(SHORTCODE_RE, (full, name: string) => {
		return resolveShortcode(name) ?? full;
	});
}

function resolveShortcode(name: string): string | undefined {
	const direct = get(name);
	if (direct !== undefined) {
		return direct;
	}
	const aliased = SLACK_ALIASES[name];
	if (aliased === undefined) {
		return undefined;
	}
	return get(aliased);
}
