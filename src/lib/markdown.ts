import MarkdownIt from 'markdown-it';
import DOMPurify from 'dompurify';

// markdown-it's types use `export =` (CommonJS), so the `Token`
// type isn't directly importable. Infer it from `md.parse()` —
// `parse` returns `Token[]`, so `ReturnType` gives us the array
// and `number` indexes the element type.
type MdToken = ReturnType<typeof md.parse>[number];
import { parse as parseYaml } from 'yaml';
import { openUrl } from '@tauri-apps/plugin-opener';
import { extractFenceLanguages, highlightCode, loadHighlighters } from './editor/highlightCode';

// Markdown rendering pipeline. Intentionally narrow: we want a
// preview that's safe to drop into `innerHTML`, not a full GitHub-
// flavored renderer. Anything fancier (math, mermaid, footnotes) is
// a follow-up — add it when someone on the team asks.
//
// A leading YAML frontmatter block (`---` … `---` at the very top of
// the file, as used by Jekyll/Hugo docs and Hub model/dataset cards)
// is split off before markdown-it sees it — otherwise the closing
// `---` reads as a setext heading and the whole block renders as one
// garbled `<h2>`. We parse it and render a small metadata table at
// the top of the preview (GitHub's behaviour); unparseable or
// non-mapping frontmatter falls back to a syntax-highlighted YAML
// block so the source is still readable. See `splitFrontmatter`.
//
// Fenced code blocks are syntax-highlighted via CodeMirror's own
// grammars (see `./editor/highlightCode.ts`). Same parser → same
// colors as the live editor.
//
// XSS posture (defense in depth):
//   1. `html: false` tells markdown-it to escape any raw HTML in the
//      source. `<script>alert(1)</script>` in the file becomes a
//      literal string, never an element.
//   2. `linkify: false` to avoid auto-linking strings the author
//      didn't intend as URLs. Manual `[text](url)` still works and
//      goes through markdown-it's URL validator, which already
//      rejects `javascript:` and `vbscript:`.
//   3. DOMPurify runs on the resulting HTML and strips anything
//      markdown-it (or our highlighter's span injection) might have
//      let through (it's been audited; we have not). We allow the
//      `class` attribute explicitly so syntax-highlighter spans
//      survive the sanitiser.
//
// We render once per source change. The component caches the result
// so toggling between Source and Preview without edits is free.

// Two parser instances differ only in whether bare URLs become
// links: file-content / docs (the default) keeps `linkify: false`
// so we don't mangle text the author didn't mean as a URL; chat
// transcripts (the `Linkified` variant, used by the coder + slack
// surfaces) opts in because the model / sender will routinely
// drop raw URLs into prose. Sharing the highlighter + link
// renderer config below keeps the two surfaces visually identical
// for everything else.
function buildMarkdownIt(linkify: boolean): MarkdownIt {
	const md = new MarkdownIt({
		html: false,
		linkify,
		breaks: false,
		typographer: false,
		// `highlight` must be synchronous. Callers preload grammars via
		// `loadHighlighters` before invoking `renderMarkdown`; inside the
		// synchronous render `highlightCode` hits the cache and emits
		// coloured HTML or returns `''` to fall back to markdown-it's
		// default `<pre><code>` rendering.
		highlight: (code, lang) => highlightCode(code, lang),
	});
	applyLinkRules(md);
	applyFenceCopyRule(md);
	applyHeadingAnchorRule(md);
	applyInlineAnchorRule(md);
	return md;
}

/**
 * GitHub-style slug for a heading text. Lower-cases, strips
 * anything that isn't an ASCII word character, dash, or space,
 * then collapses runs of whitespace / dashes into single dashes.
 *
 * Exported for tests and for the inline `<a name=…>` rule which
 * normalises the captured anchor name the same way headings do —
 * so authors can refer to either with the same `#fragment` syntax
 * without having to remember which side normalised what.
 *
 * Non-ASCII letters (CJK, accented latin, emoji) are deliberately
 * stripped rather than transliterated. Authors who want a
 * predictable anchor on a non-ASCII heading should add an explicit
 * `<a name="…"></a>` next to it.
 */
export function slugifyHeading(text: string): string {
	return text
		.toLowerCase()
		.replace(/[^\w\- ]+/g, '')
		.trim()
		.replace(/[\s-]+/g, '-');
}

/**
 * Emit `id="…"` on every heading so `[link](#section-title)` and
 * the browser's native fragment scroll just work. Slugs are
 * de-duplicated within a single document by suffixing `-1`, `-2`,
 * … on collisions (GitHub's behaviour). The first occurrence is
 * unsuffixed so existing inbound links keep resolving even after
 * a second heading with the same text appears.
 */
function applyHeadingAnchorRule(parser: MarkdownIt): void {
	parser.core.ruler.push('heading_anchors', (state) => {
		const seen = new Map<string, number>();
		for (let i = 0; i < state.tokens.length; i++) {
			const token = state.tokens[i];
			if (!token || token.type !== 'heading_open') {
				continue;
			}
			const inline = state.tokens[i + 1];
			if (!inline || inline.type !== 'inline') {
				continue;
			}
			const base = slugifyHeading(inline.content);
			if (base === '') {
				continue;
			}
			const count = seen.get(base) ?? 0;
			seen.set(base, count + 1);
			const slug = count === 0 ? base : `${base}-${count}`;
			if (token.attrIndex('id') < 0) {
				token.attrPush(['id', slug]);
			}
		}
		return false;
	});
}

/**
 * Recognise inline `<a name="…"></a>` and `<a id="…"></a>` tags
 * in the markdown source and emit them as real anchor elements.
 *
 * We otherwise run with `html: false` so arbitrary raw HTML in the
 * source escapes to literal text — that's the first XSS layer. The
 * narrow exception for empty named anchors is safe because the
 * inline rule extracts only the name itself, slugifies it the same
 * way headings do, and re-emits a clean `<a id="…"></a>` token. No
 * other attributes survive, the tag must be empty (no inner HTML),
 * and DOMPurify still scrubs the result. The author's intent —
 * "place a link target here" — is preserved without widening the
 * raw-HTML surface to anything else.
 */
function applyInlineAnchorRule(parser: MarkdownIt): void {
	parser.inline.ruler.before('html_inline', 'named_anchor', (state, silent) => {
		const src = state.src;
		const start = state.pos;
		if (src.charCodeAt(start) !== 0x3c /* < */) {
			return false;
		}
		// Sticky regex anchored at the cursor — `y` flag means
		// `lastIndex` controls where matching starts and the regex
		// fails fast if the pattern doesn't fit at exactly that
		// offset. No backtracking through the rest of the string.
		ANCHOR_RE.lastIndex = start;
		const match = ANCHOR_RE.exec(src);
		if (!match) {
			return false;
		}
		const name = match[1] ?? '';
		if (name === '') {
			return false;
		}
		if (!silent) {
			const slug = slugifyHeading(name) || name;
			const token = state.push('html_inline', '', 0);
			token.content = `<a id="${escapeHtmlAttr(slug)}"></a>`;
		}
		state.pos = start + match[0].length;
		return true;
	});
}

// Sticky (`y`) so we only match starting exactly at `lastIndex`.
// Whitespace is generous inside the tag; the outer shape is fixed:
// opening tag with a `name` or `id` attribute, immediately closed
// (`</a>` or self-closing). No other attributes.
const ANCHOR_RE = /<a\s+(?:name|id)\s*=\s*"([^"<>]*)"\s*(?:\/\s*>|>\s*<\/a\s*>)/iy;

function escapeHtmlAttr(value: string): string {
	return value.replace(/&/g, '&amp;').replace(/"/g, '&quot;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
}

// Wrap every fenced code block so a hover-revealed "Copy" button
// can sit in the top-right corner. The click is delegated to the
// hosting `<article>` (see `handleMarkdownCopyClick`) which finds
// the sibling `<pre>` and writes its `textContent` to the
// clipboard. We don't touch indented-code blocks (`code_block`
// token) — those are a markdown rarity nowadays and the model
// always emits fences anyway, so the maintenance cost of styling
// two copy-button shapes isn't worth it.
function applyFenceCopyRule(parser: MarkdownIt): void {
	const defaultFenceRender =
		parser.renderer.rules.fence ?? ((tokens, idx, options, _env, self) => self.renderToken(tokens, idx, options));
	parser.renderer.rules.fence = (tokens, idx, options, env, self) => {
		const fence = defaultFenceRender(tokens, idx, options, env, self);
		return `<div class="md-code-block">${fence}<button class="md-copy-code" type="button" aria-label="Copy code">Copy</button></div>`;
	};
}

const md = buildMarkdownIt(false);
const mdLinkified = buildMarkdownIt(true);

// Force every link to open in a new context and carry safe `rel`
// attributes. Prevents `target="_blank"` reverse-tabnabbing for
// links that opt into a new tab via reference syntax, and makes
// click-through behaviour predictable inside the IDE webview.
function applyLinkRules(parser: MarkdownIt): void {
	const defaultLinkRender =
		parser.renderer.rules.link_open ?? ((tokens, idx, options, _env, self) => self.renderToken(tokens, idx, options));
	parser.renderer.rules.link_open = (tokens, idx, options, env, self) => {
		const token = tokens[idx];
		if (token) {
			const safeRel = 'noopener noreferrer';
			const relIdx = token.attrIndex('rel');
			if (relIdx < 0) {
				token.attrPush(['rel', safeRel]);
			} else if (token.attrs) {
				const attr = token.attrs[relIdx];
				if (attr) {
					attr[1] = safeRel;
				}
			}
		}
		return defaultLinkRender(tokens, idx, options, env, self);
	};
}

/**
 * Module-level memo of rendered markdown. Folder-switch profiling
 * (see test-plan 0076) traced a ~270 ms style recalc per swap back
 * to the cascade of `{@html html}` updates that fire when many
 * `CoderMarkdown` instances mount at once: each one schedules an
 * `rAF`, the rAFs all fire in the same frame, every async render
 * resolves around the same time, and the DOM ends up with N
 * subtrees swapped in close succession. Memoising the rendered
 * HTML lets `CoderMarkdown` skip the rAF + async dance entirely
 * on a cache hit (folder swap back to an already-visited session,
 * reopening a session, re-mounting the panel) and apply the cached
 * HTML synchronously during the same Svelte flush as the row mount.
 *
 * Key is `linkify`-tagged so the two parser modes (file content
 * vs. chat transcript) don't collide. Eviction is FIFO at
 * `MARKDOWN_CACHE_MAX` entries; raw markdown source rarely exceeds
 * a few kilobytes, so the steady-state memory cap is small (a few
 * MB worst case) and the cache resets on page reload.
 */
const markdownCache = new Map<string, string>();
const MARKDOWN_CACHE_MAX = 500;

function markdownCacheKey(source: string, linkify: boolean): string {
	return (linkify ? 'L\x00' : '_\x00') + source;
}

/**
 * Sync lookup against the render cache. Returns `undefined` for a
 * miss — caller falls back to `renderMarkdown` (async).
 */
export function getCachedMarkdown(source: string, options: { linkify?: boolean } = {}): string | undefined {
	return markdownCache.get(markdownCacheKey(source, options.linkify ?? false));
}

/**
 * A single rendered top-level markdown block — one paragraph,
 * heading, fenced code block, list, blockquote, table, etc. — with
 * a stable key for Svelte's keyed `{#each}`.
 *
 * `renderMarkdownBlocks` splits the parsed token stream at block
 * boundaries and renders each block independently. During streaming,
 * only the last (still-growing) block changes between deltas; all
 * earlier blocks are "frozen" — their source text, tokens, and
 * rendered HTML are identical. Svelte's keyed `{@html}` effect sees
 * the same HTML string for a frozen block and skips the `innerHTML`
 * write, so frozen blocks' DOM nodes are never touched mid-stream.
 * This is what eliminates the flicker that a whole-document
 * `{@html}` rebuild causes.
 */
export type MarkdownBlock = {
	key: string;
	html: string;
};

/**
 * Per-block rendered-HTML cache. Keyed by the block's source text
 * (extracted from the token `map` line range) and its index in the
 * document — the index disambiguates two blocks with the same source
 * text whose HTML differs (e.g. duplicate headings whose `id` slugs
 * are suffix-de-duplicated document-wide).
 *
 * The whole-source `markdownCache` above can't help during streaming:
 * every delta changes the full source string, so every delta is a
 * whole-source cache miss. The per-block cache isolates the frozen
 * blocks from the live tail.
 */
const blockHtmlCache = new Map<string, string>();
const BLOCK_HTML_CACHE_MAX = 2000;

/**
 * Whole-source → block-array cache. Lets `CoderMarkdown.svelte`'s
 * sync fast path (folder swap, re-mount) skip the async
 * `renderMarkdownBlocks` dance entirely when the full source was
 * rendered earlier in the session.
 */
const blockArrayCache = new Map<string, MarkdownBlock[]>();
const BLOCK_ARRAY_CACHE_MAX = 500;

/**
 * Detect a leading YAML frontmatter block and split it from the
 * markdown body. The block must start at the very first byte (an
 * optional BOM aside): a line containing only `---`, terminated by a
 * later line of only `---` or `...`. Returns `frontmatter: null` when
 * the source has no such block, in which case `body === source`.
 *
 * Deliberately conservative — a stray `---` further down the document
 * is a horizontal rule (or a setext heading underline), never a
 * frontmatter fence.
 */
export function splitFrontmatter(source: string): { frontmatter: string | null; body: string } {
	const match = FRONTMATTER_RE.exec(source);
	if (!match) {
		return { frontmatter: null, body: source };
	}
	return { frontmatter: match[1] ?? '', body: source.slice(match[0].length) };
}

// Opening fence at offset 0 (optional UTF-8 BOM), body captured
// lazily, closing fence (`---` or `...`) on its own line. The `\r?\n`
// before the closing fence anchors it to a line start without needing
// the `m` flag.
const FRONTMATTER_RE = /^\uFEFF?---[ \t]*\r?\n([\s\S]*?)\r?\n(?:---|\.\.\.)[ \t]*(?:\r?\n|$)/;

/**
 * Render a parsed frontmatter block to HTML. A mapping becomes a
 * borderless key/value table (GitHub's convention); anything else —
 * a top-level sequence, a bare scalar, or YAML that fails to parse —
 * falls back to the raw source in a syntax-highlighted YAML block so
 * the author still sees their metadata.
 *
 * We parse with the `failsafe` schema so every scalar stays a string:
 * the table is for display, and coercing `version: 1.0` to the number
 * `1` or `date: 2024-01-01` to a `Date` object would misrepresent the
 * source. Output is escaped here and sanitised again by DOMPurify.
 */
function frontmatterToHtml(raw: string): string {
	let data: unknown;
	try {
		data = parseYaml(raw, { schema: 'failsafe' });
	} catch {
		data = undefined;
	}
	if (isPlainRecord(data)) {
		const rows = Object.entries(data)
			.map(
				([key, value]) =>
					`<tr><th scope="row">${escapeHtmlAttr(key)}</th><td>${renderFrontmatterValue(value)}</td></tr>`,
			)
			.join('');
		if (rows !== '') {
			return `<table class="md-frontmatter"><tbody>${rows}</tbody></table>`;
		}
	}
	// `highlightCode` returns '' if the YAML grammar wasn't preloaded
	// (it always is on the render path — see `renderMarkdown`); the
	// plain `<pre>` keeps the source readable either way.
	const highlighted = highlightCode(raw, 'yaml');
	const block =
		highlighted !== ''
			? highlighted
			: `<pre class="cm-code"><code class="language-yaml">${escapeHtmlAttr(raw)}</code></pre>`;
	return `<div class="md-frontmatter md-frontmatter-raw">${block}</div>`;
}

function renderFrontmatterValue(value: unknown): string {
	if (value === null || value === undefined || value === '') {
		return '<span class="md-frontmatter-empty">—</span>';
	}
	if (Array.isArray(value)) {
		if (value.length === 0) {
			return '<span class="md-frontmatter-empty">—</span>';
		}
		// Scalar lists (tags, languages, …) render as inline chips;
		// anything richer is dumped as indented JSON so nested
		// structure stays legible without a recursive table renderer.
		if (value.every(isScalar)) {
			return value.map((item) => `<code>${escapeHtmlAttr(String(item))}</code>`).join(' ');
		}
		return `<code class="md-frontmatter-nested">${escapeHtmlAttr(JSON.stringify(value, null, 2))}</code>`;
	}
	if (typeof value === 'string') {
		return escapeHtmlAttr(value);
	}
	if (typeof value === 'number' || typeof value === 'boolean' || typeof value === 'bigint') {
		return escapeHtmlAttr(String(value));
	}
	// `failsafe` YAML only yields strings / maps / sequences, so this
	// branch is effectively unreachable — but stay total rather than
	// stringifying an object to `[object Object]`.
	return `<code class="md-frontmatter-nested">${escapeHtmlAttr(JSON.stringify(value, null, 2))}</code>`;
}

function isScalar(value: unknown): boolean {
	return value === null || typeof value === 'string' || typeof value === 'number' || typeof value === 'boolean';
}

function isPlainRecord(value: unknown): value is Record<string, unknown> {
	return typeof value === 'object' && value !== null && !Array.isArray(value);
}

/**
 * Render a Markdown string to sanitised HTML. Async because the
 * syntax-highlighter pre-loads the CodeMirror grammar for every
 * fenced-code language before the synchronous render — dynamic
 * imports can't happen mid-render.
 *
 * Typical call sites (`MarkdownView.svelte`, LSP hover popover) are
 * already async, so the Promise is cheap. A second render for the
 * same set of fence languages short-circuits immediately because
 * the parser cache is hot.
 *
 * `linkify`: turn bare URLs / emails into clickable links. Off
 * for file content (the markdown author already wrote `[text](url)`
 * for things they meant as links); on for chat-style transcripts
 * where raw URLs in prose are the norm. Default is off so any
 * existing caller keeps the old behaviour without thinking about
 * the flag.
 *
 * The rendered HTML is stored in `markdownCache`; subsequent calls
 * for the same `(source, linkify)` short-circuit on the synchronous
 * `getCachedMarkdown` path used by `CoderMarkdown.svelte`.
 */
export async function renderMarkdown(source: string, options: { linkify?: boolean } = {}): Promise<string> {
	const linkify = options.linkify ?? false;
	const key = markdownCacheKey(source, linkify);
	const cached = markdownCache.get(key);
	if (cached !== undefined) {
		return cached;
	}
	const { frontmatter, body } = splitFrontmatter(source);
	const langs = extractFenceLanguages(body);
	if (frontmatter !== null) {
		langs.push('yaml');
	}
	await loadHighlighters(langs);
	const parser = linkify ? mdLinkified : md;
	const html = (frontmatter !== null ? frontmatterToHtml(frontmatter) : '') + parser.render(body);
	const sanitised = DOMPurify.sanitize(html, {
		// Block any URI scheme that isn't on the known-safe list.
		// DOMPurify defaults already cover the common cases; this is
		// belt-and-suspenders. `data:image/*` stays allowed (used by
		// embedded PNGs); arbitrary `data:text/html` does not.
		ALLOW_UNKNOWN_PROTOCOLS: false,
		// Always return a string, never a DOM node. We assign to
		// `innerHTML` so a string is what we want.
		RETURN_TRUSTED_TYPE: false,
		// `<button>` is on DOMPurify's default allow-list but the
		// `type` attribute isn't always — passing it explicitly so
		// our fenced-code "Copy" buttons are non-submitting buttons
		// regardless of the surrounding form context.
		ADD_ATTR: ['type'],
	});
	markdownCache.set(key, sanitised);
	if (markdownCache.size > MARKDOWN_CACHE_MAX) {
		const oldest = markdownCache.keys().next().value;
		if (oldest !== undefined) {
			markdownCache.delete(oldest);
		}
	}
	return sanitised;
}

/**
 * Split the flat markdown-it token array into top-level blocks. A
 * block is a maximal run of tokens that starts at `level === 0` and
 * continues until `level` returns to `0` after a close (or, for
 * self-closing tokens like `fence` / `hr` / `code_block`, just that
 * single token).
 *
 * Once markdown-it closes a top-level block and moves on, appending
 * more source can never retroactively change it — no backtracking.
 * Only the last block is "live" and still growing during a stream.
 * That invariant is what makes per-block caching safe: a frozen
 * block's source text, tokens, and rendered HTML are identical
 * across deltas, so its cache entry is a permanent hit.
 *
 * The `map` property on block-opening tokens gives the `[startLine,
 * endLine]` range in the source, which we use to extract the block's
 * raw text for the cache key. Self-closing tokens (fence, hr, …)
 * also carry `map`.
 */
function splitTopLevelBlocks(tokens: MdToken[], source: string): { text: string; tokens: MdToken[] }[] {
	const blocks: { text: string; tokens: MdToken[] }[] = [];
	const lines = source.split('\n');
	let i = 0;
	while (i < tokens.length) {
		const token = tokens[i];
		if (!token) {
			i++;
			continue;
		}
		// Only level-0 tokens start a top-level block.
		if (token.level !== 0) {
			i++;
			continue;
		}
		if (token.nesting === 0) {
			// Self-closing: fence, hr, code_block, reference, etc.
			const text = extractTokenText(token, lines);
			blocks.push({ text, tokens: [token] });
			i++;
			continue;
		}
		if (token.nesting === 1) {
			// Opening token — collect until the matching close.
			const start = i;
			let depth = 1;
			i++;
			while (i < tokens.length && depth > 0) {
				const t = tokens[i];
				if (!t) {
					i++;
					continue;
				}
				if (t.nesting === 1) {
					depth++;
				} else if (t.nesting === -1) {
					depth--;
				}
				i++;
			}
			const blockTokens = tokens.slice(start, i);
			const text = extractTokenText(token, lines);
			blocks.push({ text, tokens: blockTokens });
			continue;
		}
		// Stray close at level 0 (shouldn't happen in well-formed
		// output, but advance to avoid an infinite loop).
		i++;
	}
	return blocks;
}

/**
 * Extract the source text spanned by a block-opening token's `map`
 * `[startLine, endLine)` range. Falls back to the joined `content`
 * of the block's tokens when `map` is absent (some synthetic tokens
 * don't carry source positions).
 */
function extractTokenText(openToken: MdToken, lines: string[]): string {
	if (openToken.map) {
		const [start, end] = openToken.map;
		return lines.slice(start, end).join('\n');
	}
	return openToken.content ?? '';
}

/**
 * Sync lookup against the block-array cache. Returns `undefined`
 * for a miss — caller falls back to `renderMarkdownBlocks` (async).
 * Used by `CoderMarkdown.svelte`'s fast path on folder swap /
 * re-mount, identical in spirit to `getCachedMarkdown`.
 */
export function getCachedMarkdownBlocks(
	source: string,
	options: { linkify?: boolean } = {},
): MarkdownBlock[] | undefined {
	return blockArrayCache.get(markdownCacheKey(source, options.linkify ?? false));
}

/**
 * Render a Markdown string to an array of independently-cached
 * top-level blocks. Each block's HTML is cached by its source text
 * and document position, so during streaming only the live tail
 * block misses the cache — all frozen blocks short-circuit on a
 * sync `Map.get`.
 *
 * The block-array itself is also cached by the full source string,
 * so the whole-source fast path (`getCachedMarkdownBlocks`) skips
 * even the token-level parse on a re-mount of an already-rendered
 * message.
 *
 * The frontmatter is prepended as a synthetic block (index 0) so
 * the keyed `{#each}` in `CoderMarkdown.svelte` can treat it
 * uniformly with body blocks.
 */
export async function renderMarkdownBlocks(
	source: string,
	options: { linkify?: boolean } = {},
): Promise<MarkdownBlock[]> {
	const linkify = options.linkify ?? false;
	const arrayKey = markdownCacheKey(source, linkify);
	const cachedArray = blockArrayCache.get(arrayKey);
	if (cachedArray !== undefined) {
		return cachedArray;
	}
	const { frontmatter, body } = splitFrontmatter(source);
	const langs = extractFenceLanguages(body);
	if (frontmatter !== null) {
		langs.push('yaml');
	}
	await loadHighlighters(langs);
	const parser = linkify ? mdLinkified : md;
	const tokens = parser.parse(body, {});
	const rawBlocks = splitTopLevelBlocks(tokens, body);
	const blocks: MarkdownBlock[] = [];
	if (frontmatter !== null) {
		blocks.push(renderFrontmatterBlock(frontmatter, 0));
	}
	for (let i = 0; i < rawBlocks.length; i++) {
		const raw = rawBlocks[i];
		if (!raw) {
			continue;
		}
		blocks.push(renderBlock(raw.tokens, raw.text, i + (frontmatter !== null ? 1 : 0), parser));
	}
	blockArrayCache.set(arrayKey, blocks);
	if (blockArrayCache.size > BLOCK_ARRAY_CACHE_MAX) {
		const oldest = blockArrayCache.keys().next().value;
		if (oldest !== undefined) {
			blockArrayCache.delete(oldest);
		}
	}
	return blocks;
}

/**
 * Render a single block's token slice to sanitised HTML. The
 * per-block cache key includes the block's source text and its
 * document index — the index disambiguates blocks whose source text
 * is identical but whose HTML differs (e.g. duplicate headings
 * whose `id` slugs are suffix-de-duplicated document-wide by the
 * heading-anchor rule).
 */
function renderBlock(tokens: MdToken[], sourceText: string, index: number, parser: MarkdownIt): MarkdownBlock {
	const key = `${index}\x00${sourceText}`;
	const cached = blockHtmlCache.get(key);
	if (cached !== undefined) {
		return { key, html: cached };
	}
	const raw = parser.renderer.render(tokens, parser.options, {});
	const sanitised = DOMPurify.sanitize(raw, {
		ALLOW_UNKNOWN_PROTOCOLS: false,
		RETURN_TRUSTED_TYPE: false,
		ADD_ATTR: ['type'],
	});
	blockHtmlCache.set(key, sanitised);
	if (blockHtmlCache.size > BLOCK_HTML_CACHE_MAX) {
		const oldest = blockHtmlCache.keys().next().value;
		if (oldest !== undefined) {
			blockHtmlCache.delete(oldest);
		}
	}
	return { key, html: sanitised };
}

/**
 * Render the frontmatter as a synthetic block at index 0.
 */
function renderFrontmatterBlock(frontmatter: string, index: number): MarkdownBlock {
	const key = `${index}\x00${frontmatter}`;
	const cached = blockHtmlCache.get(key);
	if (cached !== undefined) {
		return { key, html: cached };
	}
	const raw = frontmatterToHtml(frontmatter);
	const sanitised = DOMPurify.sanitize(raw, {
		ALLOW_UNKNOWN_PROTOCOLS: false,
		RETURN_TRUSTED_TYPE: false,
		ADD_ATTR: ['type'],
	});
	blockHtmlCache.set(key, sanitised);
	if (blockHtmlCache.size > BLOCK_HTML_CACHE_MAX) {
		const oldest = blockHtmlCache.keys().next().value;
		if (oldest !== undefined) {
			blockHtmlCache.delete(oldest);
		}
	}
	return { key, html: sanitised };
}

/**
 * Click delegate for the "Copy" buttons rendered inside fenced
 * code blocks. Returns `true` if the click was handled (so the
 * caller can `event.preventDefault()` and stop further routing),
 * `false` otherwise — the caller falls through to its anchor /
 * link logic in that case.
 *
 * The button text flips to "Copied" for a beat after a successful
 * write so the user gets visual feedback in a webview where
 * "did the clipboard actually take?" is otherwise invisible.
 * Failure mode (clipboard API unavailable, permission denied,
 * etc.): the text flips to "Failed"; we don't surface a toast
 * because the button itself is the affordance.
 */
export function handleMarkdownCopyClick(event: MouseEvent): boolean {
	const target = event.target;
	if (!(target instanceof HTMLElement)) {
		return false;
	}
	const button = target.closest('.md-copy-code');
	if (!(button instanceof HTMLButtonElement)) {
		return false;
	}
	event.preventDefault();
	const wrap = button.parentElement;
	const pre = wrap?.querySelector('pre');
	const code = pre?.textContent ?? '';
	if (code === '') {
		return true;
	}
	void copyTextWithFeedback(button, code, 'Copy', 'Copied', 'Failed');
	return true;
}

async function copyTextWithFeedback(
	button: HTMLButtonElement,
	text: string,
	idleLabel: string,
	successLabel: string,
	failureLabel: string,
): Promise<void> {
	let ok = false;
	try {
		await navigator.clipboard.writeText(text);
		ok = true;
	} catch {
		ok = false;
	}
	button.textContent = ok ? successLabel : failureLabel;
	// Reset after ~1.2s. Long enough to register, short enough that
	// rapid re-clicks see the live state again.
	window.setTimeout(() => {
		button.textContent = idleLabel;
	}, 1200);
}

/**
 * Schemes whose links we route to the OS default app via the Tauri
 * opener plugin. Anything else (file:, javascript:, custom
 * protocols, bare relative paths) is handled by the caller or
 * silently swallowed — never followed as a raw navigation inside
 * the Tauri webview, which would replace the IDE shell with the
 * target page.
 *
 * Keep this list in sync with the `opener:default` capability set.
 */
export const EXTERNAL_MARKDOWN_SCHEMES = new Set(['http:', 'https:', 'mailto:', 'tel:']);

/**
 * Test-only access to internals. Tests construct their own
 * `MarkdownIt` to skip the highlighter (grammar imports break in
 * the vitest environment) and apply just the rules under test.
 */
export const __test = { applyHeadingAnchorRule, applyInlineAnchorRule, frontmatterToHtml, splitTopLevelBlocks };

/**
 * If `href` parses as an absolute URL with an allow-listed scheme,
 * open it via the Tauri opener plugin and return `true`. Returns
 * `false` for in-page fragments (`#foo`), relative paths, and
 * schemes that aren't in [`EXTERNAL_MARKDOWN_SCHEMES`] — the caller
 * decides what to do with those.
 *
 * Shared by the Markdown file preview (`MarkdownView.svelte`) and
 * the LSP hover popover (`editor/lsp.ts`) so both render paths end
 * up with identical click semantics: MDN references, `rust-analyzer`
 * doc links, `@link` crossrefs in JS/TS tooltips all open in the
 * user's browser instead of navigating the IDE window.
 */
export function openExternalMarkdownLink(href: string): boolean {
	let url: URL;
	try {
		url = new URL(href);
	} catch {
		return false;
	}
	if (!EXTERNAL_MARKDOWN_SCHEMES.has(url.protocol)) {
		return false;
	}
	void openUrl(url.toString());
	return true;
}

/**
 * Resolve a relative (or workspace-root-absolute) link from inside a
 * markdown file to a workspace-relative path, mirroring how a browser
 * resolves URLs against the document's base. Returns `null` when the
 * link can't be resolved within the workspace — empty href, escapes
 * the root via `..`, or invalid `%`-encoding.
 *
 * Conventions:
 *   - `./foo.md` and `foo.md` resolve relative to the current file's
 *     directory, like a normal browser would.
 *   - `/foo.md` is treated as workspace-root-absolute. Markdown
 *     authors writing `[…](/something)` mean "from the project root",
 *     not the filesystem root — those are the same thing inside the
 *     IDE because the host already pins paths under the workspace
 *     root anyway.
 *   - `?query` and `#fragment` are stripped before resolution; the
 *     fragment is dropped on the floor for now (cross-file anchor
 *     scroll is a follow-up). Same-document fragments — including
 *     auto-generated heading slugs and inline `<a name="…">` /
 *     `<a id="…">` anchors — work directly via the browser's
 *     fragment scroll, no IPC needed.
 *   - The host re-validates path boundaries on the first IPC call, so
 *     this function is only the first line of defence.
 */
export function resolveMarkdownLink(currentPath: string, href: string): string | null {
	// Strip the fragment first so `?query=foo#bar` only loses the
	// fragment (matches browser behavior); query then drops too.
	const withoutFragment = href.split('#')[0] ?? '';
	const withoutQuery = withoutFragment.split('?')[0] ?? '';
	if (!withoutQuery) {
		return null;
	}
	let decoded: string;
	try {
		decoded = decodeURIComponent(withoutQuery);
	} catch {
		return null;
	}

	// Build the base segment list. Workspace-root-absolute links bypass
	// the current file's directory entirely; otherwise we splice the
	// link into wherever the current file sits.
	const segments: string[] = [];
	if (decoded.startsWith('/')) {
		segments.push(...decoded.split('/').filter(Boolean));
	} else {
		const slash = currentPath.lastIndexOf('/');
		const dir = slash >= 0 ? currentPath.slice(0, slash) : '';
		if (dir) {
			segments.push(...dir.split('/').filter(Boolean));
		}
		segments.push(...decoded.split('/').filter(Boolean));
	}

	const resolved: string[] = [];
	for (const segment of segments) {
		if (segment === '.') {
			continue;
		}
		if (segment === '..') {
			if (resolved.length === 0) {
				return null;
			}
			resolved.pop();
			continue;
		}
		resolved.push(segment);
	}
	if (resolved.length === 0) {
		return null;
	}
	return resolved.join('/');
}
