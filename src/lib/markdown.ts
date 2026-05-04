import MarkdownIt from 'markdown-it';
import DOMPurify from 'dompurify';
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

const md = new MarkdownIt({
	html: false,
	linkify: false,
	breaks: false,
	typographer: false,
	// `highlight` must be synchronous. Callers preload grammars via
	// `loadHighlighters` before invoking `renderMarkdown`; inside the
	// synchronous render `highlightCode` hits the cache and emits
	// coloured HTML or returns `''` to fall back to markdown-it's
	// default `<pre><code>` rendering.
	highlight: (code, lang) => highlightCode(code, lang),
});

// Force every link to open in a new context and carry safe `rel`
// attributes. Prevents `target="_blank"` reverse-tabnabbing for
// links that opt into a new tab via reference syntax, and makes
// click-through behaviour predictable inside the IDE webview.
const defaultLinkRender =
	md.renderer.rules.link_open ?? ((tokens, idx, options, _env, self) => self.renderToken(tokens, idx, options));
md.renderer.rules.link_open = (tokens, idx, options, env, self) => {
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
 */
export async function renderMarkdown(source: string): Promise<string> {
	await loadHighlighters(extractFenceLanguages(source));
	const html = md.render(source);
	return DOMPurify.sanitize(html, {
		// Block any URI scheme that isn't on the known-safe list.
		// DOMPurify defaults already cover the common cases; this is
		// belt-and-suspenders. `data:image/*` stays allowed (used by
		// embedded PNGs); arbitrary `data:text/html` does not.
		ALLOW_UNKNOWN_PROTOCOLS: false,
		// Always return a string, never a DOM node. We assign to
		// `innerHTML` so a string is what we want.
		RETURN_TRUSTED_TYPE: false,
	});
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
