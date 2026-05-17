import MarkdownIt from 'markdown-it';
import DOMPurify from 'dompurify';
import { openUrl } from '@tauri-apps/plugin-opener';
import { extractFenceLanguages, highlightCode, loadHighlighters } from './editor/highlightCode';

// Markdown rendering pipeline. Intentionally narrow: we want a
// preview that's safe to drop into `innerHTML`, not a full GitHub-
// flavored renderer. Anything fancier (math, mermaid, footnotes) is
// a follow-up — add it when someone on the team asks.
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
	return md;
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
	await loadHighlighters(extractFenceLanguages(source));
	const parser = linkify ? mdLinkified : md;
	const html = parser.render(source);
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
 *     fragment is dropped on the floor for now (anchor-scroll inside
 *     a freshly-opened file is a follow-up — the renderer doesn't
 *     emit heading anchors yet either).
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
