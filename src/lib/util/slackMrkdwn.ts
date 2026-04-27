// Tokenizer for Slack's [`mrkdwn`][1] message format.
//
// Slack's flavour is *not* CommonMark — `markdown-it` would mis-render
// every other token. Specifically, Slack uses single asterisks for bold,
// single underscores for italic, single tildes for strikethrough, and
// puts every link / mention / channel / broadcast / date inside angle
// brackets (e.g. `<@U123>`, `<https://example.com|label>`). Source text
// is HTML-encoded before transit, so `<`, `>` and `&` only appear as
// `&lt;`, `&gt;` and `&amp;` outside structured tokens — which means a
// literal `<` always opens a structured token.
//
// Output is a tree of [`BlockNode`]s; the renderer
// (`SlackMessageBody.svelte`) walks it and emits Svelte. Mentions
// resolve via the [`SlackPanelState.peekUser`] cache so the tree
// only carries the raw user ID — no async work in the parser.
//
// What this parser deliberately *does not* handle (yet):
// - Custom emoji (`:tada:`) — passed through as text, font handles them.
// - Lists / headers — Slack mrkdwn doesn't support them.
// - Channel name resolution — we trust Slack's cached label
//   (`<#C123|general>`) and fall back to `#C123` when missing.
// - Files / images / blocks (Block Kit) — bot replies in DM rarely use
//   them; revisit if needed.
//
// [1]: https://api.slack.com/reference/surfaces/formatting

export type InlineNode =
	| { type: 'text'; value: string }
	| { type: 'bold'; children: InlineNode[] }
	| { type: 'italic'; children: InlineNode[] }
	| { type: 'strike'; children: InlineNode[] }
	| { type: 'code'; value: string }
	| { type: 'link'; url: string; label: string | null }
	| { type: 'userMention'; userId: string; label: string | null }
	| { type: 'channelMention'; channelId: string; label: string | null }
	| { type: 'broadcast'; kind: 'here' | 'channel' | 'everyone'; label: string | null }
	| { type: 'usergroup'; id: string; label: string | null }
	| { type: 'date'; fallback: string };

export type BlockNode =
	| { type: 'text'; children: InlineNode[] }
	| { type: 'codeblock'; value: string }
	| { type: 'quote'; children: InlineNode[] };

/**
 * Top-level entry point. Splits the message into block-level chunks
 * (` ``` ` fenced code, `>`-prefixed quotes, plain runs) and recurses
 * into the inline parser for each text/quote chunk. Pure; safe to call
 * during render.
 */
export function parseSlackMrkdwn(raw: string): BlockNode[] {
	const blocks: BlockNode[] = [];
	let cursor = 0;
	while (cursor < raw.length) {
		const fenceStart = raw.indexOf('```', cursor);
		if (fenceStart === -1) {
			pushTextSegment(blocks, raw.slice(cursor));
			break;
		}
		if (fenceStart > cursor) {
			pushTextSegment(blocks, raw.slice(cursor, fenceStart));
		}
		const fenceEnd = raw.indexOf('```', fenceStart + 3);
		if (fenceEnd === -1) {
			// Unclosed fence: Slack treats the leading ``` as literal
			// text, not the start of code. Same here so we don't swallow
			// the rest of the message into a fake block.
			pushTextSegment(blocks, raw.slice(fenceStart));
			break;
		}
		const code = raw.slice(fenceStart + 3, fenceEnd);
		blocks.push({ type: 'codeblock', value: decodeEntities(stripFenceLeadingNewline(code)) });
		cursor = fenceEnd + 3;
	}
	return blocks;
}

/**
 * Group consecutive lines into quote / text blocks. A line is a quote
 * line iff it starts with `>` (Slack's mrkdwn syntax). The single
 * leading `>` (and one optional space) is stripped before re-joining.
 */
function pushTextSegment(blocks: BlockNode[], segment: string): void {
	if (segment.length === 0) {
		return;
	}
	const lines = segment.split('\n');
	let mode: 'text' | 'quote' = 'text';
	let buf: string[] = [];
	const flush = () => {
		if (buf.length === 0) {
			return;
		}
		const joined = buf.join('\n');
		if (joined.length === 0) {
			buf = [];
			return;
		}
		const inline = parseInline(joined);
		if (mode === 'quote') {
			blocks.push({ type: 'quote', children: inline });
		} else {
			blocks.push({ type: 'text', children: inline });
		}
		buf = [];
	};
	for (const line of lines) {
		const isQuote = line.startsWith('>');
		const lineMode: 'text' | 'quote' = isQuote ? 'quote' : 'text';
		if (lineMode !== mode) {
			flush();
			mode = lineMode;
		}
		if (isQuote) {
			buf.push(line.startsWith('> ') ? line.slice(2) : line.slice(1));
		} else {
			buf.push(line);
		}
	}
	flush();
}

/** Trim one leading newline that Slack's UI adds after the opening fence. */
function stripFenceLeadingNewline(code: string): string {
	if (code.startsWith('\r\n')) {
		return code.slice(2);
	}
	if (code.startsWith('\n')) {
		return code.slice(1);
	}
	return code;
}

/**
 * Parse one line's worth of inline content. Two passes:
 *
 *  1. Walk the text and extract *atoms* — `<...>` structured tokens
 *     and inline `` `code` `` spans — replacing each with a Private
 *     Use Area placeholder of the form `\uE000<index>\uE000`. Atoms
 *     are stored in an indexed array.
 *
 *  2. Run [`parseFormatting`] on the placeholder string. Placeholders
 *     are non-word + non-whitespace characters, so they read as
 *     opaque "content" to the opener / closer rules: a `*…*` pair
 *     happily wraps a placeholder, which means `` *`code`* `` parses
 *     as bold([code]) instead of stranding the asterisks. Same trick
 *     handles `*foo <@U1> bar*` and `_a `b` c_`.
 *
 *  3. Walk the formatted tree and re-inject the atoms into text leaves
 *     by splitting on the placeholder pattern.
 *
 * This is the same placeholder dance that markdown-it and the bot's
 * own `markdownToSlack` use to keep code spans opaque to surrounding
 * formatting.
 */
function parseInline(text: string): InlineNode[] {
	const atoms: InlineNode[] = [];
	let mapped = '';
	let i = 0;
	while (i < text.length) {
		const ch = text.charAt(i);
		if (ch === '<') {
			const end = text.indexOf('>', i + 1);
			if (end === -1) {
				mapped += ch;
				i += 1;
				continue;
			}
			const inner = text.slice(i + 1, end);
			const node = parseAngleToken(inner);
			if (node === null) {
				// Unknown shape (`<some.literal>`). Keep the original
				// braces visible so the user sees what Slack sent.
				mapped += text.slice(i, end + 1);
				i = end + 1;
				continue;
			}
			mapped += pushAtom(atoms, node);
			i = end + 1;
			continue;
		}
		if (ch === '`') {
			const closeAt = findInlineCodeClose(text, i + 1);
			if (closeAt === -1) {
				mapped += ch;
				i += 1;
				continue;
			}
			const inner = text.slice(i + 1, closeAt);
			if (inner.length === 0) {
				// `` matches an empty pair; keep as literal so it doesn't
				// collapse to nothing.
				mapped += '``';
				i = closeAt + 1;
				continue;
			}
			mapped += pushAtom(atoms, { type: 'code', value: decodeEntities(inner) });
			i = closeAt + 1;
			continue;
		}
		mapped += ch;
		i += 1;
	}
	const formatted = parseFormatting(mapped);
	return rehydrateAtoms(formatted, atoms);
}

/**
 * Single character from the Unicode Private Use Area. Slack messages
 * never carry these, so collisions are impossible in practice — and
 * if a real PUA char ever sneaks through it would just look like a
 * stray atom marker and render as text, not crash.
 */
const ATOM_MARKER = '\uE000';

function pushAtom(atoms: InlineNode[], node: InlineNode): string {
	atoms.push(node);
	return `${ATOM_MARKER}${atoms.length - 1}${ATOM_MARKER}`;
}

const ATOM_RE = new RegExp(`${ATOM_MARKER}(\\d+)${ATOM_MARKER}`, 'g');

function rehydrateAtoms(nodes: InlineNode[], atoms: InlineNode[]): InlineNode[] {
	if (atoms.length === 0) {
		return nodes;
	}
	const out: InlineNode[] = [];
	for (const node of nodes) {
		if (node.type === 'text') {
			out.push(...rehydrateText(node.value, atoms));
			continue;
		}
		if (node.type === 'bold' || node.type === 'italic' || node.type === 'strike') {
			out.push({ type: node.type, children: rehydrateAtoms(node.children, atoms) });
			continue;
		}
		out.push(node);
	}
	return out;
}

function rehydrateText(text: string, atoms: InlineNode[]): InlineNode[] {
	const out: InlineNode[] = [];
	let last = 0;
	for (const match of text.matchAll(ATOM_RE)) {
		const start = match.index;
		if (start > last) {
			out.push({ type: 'text', value: text.slice(last, start) });
		}
		const idx = Number.parseInt(match[1] ?? '', 10);
		const atom = atoms[idx];
		if (atom !== undefined) {
			out.push(atom);
		}
		last = start + match[0].length;
	}
	if (last === 0) {
		return [{ type: 'text', value: text }];
	}
	if (last < text.length) {
		out.push({ type: 'text', value: text.slice(last) });
	}
	return out;
}

/** Single-backtick close: same line, not a backtick itself, no nested. */
function findInlineCodeClose(text: string, start: number): number {
	for (let i = start; i < text.length; i++) {
		const ch = text.charAt(i);
		if (ch === '\n') {
			return -1;
		}
		if (ch === '`') {
			return i;
		}
	}
	return -1;
}

/**
 * Classify the contents of a `<...>` token. Returns `null` for
 * unrecognised shapes — the caller will emit them verbatim.
 *
 * Slack's tokens (per their formatting reference):
 * - `<@U…>` / `<@U…|label>`               — user mention
 * - `<#C…>` / `<#C…|label>`               — channel mention
 * - `<!here>`, `<!channel>`, `<!everyone>` — broadcast (with optional `|label`)
 * - `<!subteam^S…>` / `<!subteam^S…|@team>` — user group
 * - `<!date^TS^FORMAT|FALLBACK>`           — formatted date (use FALLBACK)
 * - `<URL>` / `<URL|label>`                — link (http/https/mailto)
 */
function parseAngleToken(inner: string): InlineNode | null {
	if (inner.length === 0) {
		return null;
	}

	const head = inner[0];

	if (head === '@') {
		const [body, label] = splitOnPipe(inner.slice(1));
		if (body.length === 0) {
			return null;
		}
		return { type: 'userMention', userId: body, label: stripLeadingAt(label) };
	}

	if (head === '#') {
		const [body, label] = splitOnPipe(inner.slice(1));
		if (body.length === 0) {
			return null;
		}
		return { type: 'channelMention', channelId: body, label: stripLeadingHash(label) };
	}

	if (head === '!') {
		return parseSpecial(inner.slice(1));
	}

	const [target, label] = splitOnPipe(inner);
	if (isLinkScheme(target)) {
		return { type: 'link', url: decodeEntities(target), label: label !== null ? decodeEntities(label) : null };
	}

	return null;
}

function parseSpecial(rest: string): InlineNode | null {
	if (rest.startsWith('subteam^')) {
		const [body, label] = splitOnPipe(rest.slice('subteam^'.length));
		if (body.length === 0) {
			return null;
		}
		return { type: 'usergroup', id: body, label };
	}
	if (rest.startsWith('date^')) {
		const [, fallback] = splitOnPipe(rest);
		// Slack always provides a fallback string for `<!date^…>` (no
		// fallback ⇒ malformed token). When missing, we render the raw
		// timestamp segment so the user sees *something*.
		const display = fallback ?? rest.slice('date^'.length);
		return { type: 'date', fallback: decodeEntities(display) };
	}
	const [body, label] = splitOnPipe(rest);
	if (body === 'here' || body === 'channel' || body === 'everyone') {
		return { type: 'broadcast', kind: body, label };
	}
	return null;
}

function splitOnPipe(s: string): [string, string | null] {
	const idx = s.indexOf('|');
	if (idx === -1) {
		return [s, null];
	}
	return [s.slice(0, idx), s.slice(idx + 1)];
}

function stripLeadingAt(label: string | null): string | null {
	if (label === null) {
		return null;
	}
	return label.startsWith('@') ? label.slice(1) : label;
}

function stripLeadingHash(label: string | null): string | null {
	if (label === null) {
		return null;
	}
	return label.startsWith('#') ? label.slice(1) : label;
}

function isLinkScheme(target: string): boolean {
	return target.startsWith('http://') || target.startsWith('https://') || target.startsWith('mailto:');
}

// --- Formatting (bold / italic / strike) -----------------------------------
//
// Recursive-descent parser. The same marker can't nest inside itself
// (Slack's parser is the same), so `disallowed` carries the set we've
// already opened. Different markers may nest freely.

type FormatMarker = 'bold' | 'italic' | 'strike';

const MARKERS: Record<string, FormatMarker> = {
	'*': 'bold',
	_: 'italic',
	'~': 'strike',
};

function parseFormatting(text: string): InlineNode[] {
	return parseFormattingInner(text, new Set());
}

function parseFormattingInner(text: string, disallowed: ReadonlySet<FormatMarker>): InlineNode[] {
	const out: InlineNode[] = [];
	let buf = '';
	let i = 0;
	const flush = () => {
		if (buf.length === 0) {
			return;
		}
		out.push({ type: 'text', value: decodeEntities(buf) });
		buf = '';
	};
	while (i < text.length) {
		const ch = text.charAt(i);
		const marker = MARKERS[ch];
		if (marker !== undefined && !disallowed.has(marker) && isOpener(text, i, ch)) {
			const close = findCloser(text, i + 1, ch);
			if (close !== -1) {
				flush();
				const inner = text.slice(i + 1, close);
				const next = new Set(disallowed);
				next.add(marker);
				out.push({ type: marker, children: parseFormattingInner(inner, next) });
				i = close + 1;
				continue;
			}
		}
		buf += ch;
		i += 1;
	}
	flush();
	return out;
}

const WORD_RE = /[A-Za-z0-9_]/;

/** A valid opener has a non-word char (or start) before it and a non-whitespace after. */
function isOpener(text: string, i: number, marker: string): boolean {
	if (i > 0) {
		const prev = text.charAt(i - 1);
		if (WORD_RE.test(prev)) {
			return false;
		}
		// Two markers in a row would mean an empty inline. Skip; the
		// second `*` will get its chance to match later.
		if (prev === marker) {
			return false;
		}
	}
	if (i + 1 >= text.length) {
		return false;
	}
	const next = text.charAt(i + 1);
	if (next === marker || /\s/.test(next)) {
		return false;
	}
	return true;
}

/** A valid closer has a non-whitespace char before it and a non-word char (or end) after. */
function findCloser(text: string, start: number, marker: string): number {
	for (let i = start; i < text.length; i++) {
		const ch = text.charAt(i);
		if (ch === '\n') {
			return -1;
		}
		if (ch !== marker) {
			continue;
		}
		const prev = text.charAt(i - 1);
		if (/\s/.test(prev)) {
			continue;
		}
		const next = i + 1 < text.length ? text.charAt(i + 1) : '';
		if (next !== '' && WORD_RE.test(next)) {
			continue;
		}
		return i;
	}
	return -1;
}

// --- Plain-text flattening -------------------------------------------------
//
// Used by the session-list preview row, which can't afford to render
// the full inline tree (no live mention resolution per row, no async).
// We walk the parsed tree and stringify it best-effort: structured
// tokens become their label (or the raw ID when no label is present).
//
// Slack truncates the preview text server-side (we ask for ~80 chars).
// When the cut lands inside a `<...>` token the closing `>` is gone
// and the tokenizer would emit the literal `<` — looks ugly. We strip
// the dangling tail before parsing and append an ellipsis instead.

export interface SlackPlainTextOptions {
	/**
	 * Optional synchronous resolver for `<@U…>` mentions. Returning
	 * `null` (or omitting) falls back to the embedded label, then the
	 * raw user ID. Stays sync on purpose — preview rendering can't
	 * await network calls.
	 */
	resolveUserId?: (userId: string) => string | null;
}

/** Best-effort plain-text rendering — no async, no DOM. */
export function slackPlainText(raw: string, opts: SlackPlainTextOptions = {}): string {
	const trimmed = trimDanglingAngle(raw);
	const blocks = parseSlackMrkdwn(trimmed);
	return blocks
		.map((block) => {
			if (block.type === 'codeblock') {
				return block.value;
			}
			return flattenInline(block.children, opts);
		})
		.join('\n');
}

/**
 * Walk the parsed tree and collect every `<@U…>` user ID. Used by the
 * renderer to know which `users.info` calls to fire from an `$effect`,
 * keeping the render path itself pure.
 *
 * Returns a deduplicated, insertion-ordered array.
 */
export function collectMentionedUserIds(blocks: BlockNode[]): string[] {
	const ids = new Set<string>();
	const walk = (nodes: InlineNode[]): void => {
		for (const node of nodes) {
			switch (node.type) {
				case 'userMention':
					ids.add(node.userId);
					break;
				case 'bold':
				case 'italic':
				case 'strike':
					walk(node.children);
					break;
				default:
					break;
			}
		}
	};
	for (const block of blocks) {
		if (block.type === 'codeblock') {
			continue;
		}
		walk(block.children);
	}
	return Array.from(ids);
}

/**
 * Slice the input at the last unclosed `<` (preview truncation). Walks
 * tokens left-to-right so a literal `>` *after* a balanced token isn't
 * treated as a closer for an earlier `<`.
 */
function trimDanglingAngle(text: string): string {
	let i = 0;
	while (i < text.length) {
		const ch = text.charAt(i);
		if (ch !== '<') {
			i += 1;
			continue;
		}
		const close = text.indexOf('>', i + 1);
		if (close === -1) {
			return text.slice(0, i).trimEnd() + '…';
		}
		i = close + 1;
	}
	return text;
}

function flattenInline(nodes: InlineNode[], opts: SlackPlainTextOptions): string {
	let out = '';
	for (const node of nodes) {
		switch (node.type) {
			case 'text':
				out += node.value;
				break;
			case 'bold':
			case 'italic':
			case 'strike':
				out += flattenInline(node.children, opts);
				break;
			case 'code':
				out += node.value;
				break;
			case 'link':
				out += node.label ?? node.url;
				break;
			case 'userMention': {
				const resolved = opts.resolveUserId?.(node.userId) ?? null;
				out += '@' + (node.label ?? resolved ?? node.userId);
				break;
			}
			case 'channelMention':
				out += '#' + (node.label ?? node.channelId);
				break;
			case 'broadcast':
				out += '@' + (node.label ?? node.kind);
				break;
			case 'usergroup':
				out += '@' + (node.label ?? node.id);
				break;
			case 'date':
				out += node.fallback;
				break;
		}
	}
	return out;
}

// --- HTML entity decoding --------------------------------------------------
//
// Slack escapes only three characters (`<`, `>`, `&`) per their docs.
// We don't ship a full HTML decoder — that would be both wasteful and a
// chance for surprises. Numeric entities (`&#NN;`) appear in some
// older messages, so we handle decimal and hex variants.

const ENTITY_RE = /&(amp|lt|gt|#(\d+)|#x([0-9a-fA-F]+));/g;

export function decodeEntities(s: string): string {
	return s.replace(ENTITY_RE, (_, name: string, dec?: string, hex?: string) => {
		if (name === 'amp') {
			return '&';
		}
		if (name === 'lt') {
			return '<';
		}
		if (name === 'gt') {
			return '>';
		}
		if (dec !== undefined) {
			const code = Number.parseInt(dec, 10);
			return Number.isFinite(code) ? String.fromCodePoint(code) : '';
		}
		if (hex !== undefined) {
			const code = Number.parseInt(hex, 16);
			return Number.isFinite(code) ? String.fromCodePoint(code) : '';
		}
		return '';
	});
}
