import MarkdownIt from 'markdown-it';
import DOMPurify from 'dompurify';

// Markdown rendering pipeline. Intentionally narrow: we want a
// preview that's safe to drop into `innerHTML`, not a full GitHub-
// flavored renderer. Anything fancier (syntax highlighting in code
// fences, math, mermaid, footnotes) is a follow-up — add it when
// someone on the team asks.
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
//      markdown-it might have let through (it's been audited; we
//      have not).
//
// We render once per source change. The component caches the result
// so toggling between Source and Preview without edits is free.

const md = new MarkdownIt({
	html: false,
	linkify: false,
	breaks: false,
	typographer: false,
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

export function renderMarkdown(source: string): string {
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
