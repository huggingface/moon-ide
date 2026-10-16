/**
 * Bare-URL detection for plain-text surfaces (user-typed coder
 * prompts) that must not be reinterpreted as markdown but should
 * still make `https://…` clickable.
 */

export type TextSegment = { kind: 'text'; text: string } | { kind: 'link'; text: string; href: string };

const URL_RE = /\bhttps?:\/\/[^\s<>"'`]+/g;

// Sentence punctuation that commonly trails a URL in prose
// ("see https://x.com/foo.") but is almost never part of it.
const TRAILING_PUNCT = /[.,;:!?*_~]+$/;

const CLOSERS: Record<string, string> = { ')': '(', ']': '[', '}': '{' };

function count(s: string, ch: string): number {
	let n = 0;
	for (const c of s) {
		if (c === ch) {
			n++;
		}
	}
	return n;
}

/**
 * Strip trailing punctuation and unbalanced closing brackets, so
 * `(see https://x.com/a)` links `https://x.com/a` while
 * `https://en.wikipedia.org/wiki/Foo_(bar)` keeps its paren.
 */
function trimUrl(url: string): string {
	let out = url;
	for (;;) {
		const before = out;
		out = out.replace(TRAILING_PUNCT, '');
		const last = out.at(-1);
		const opener = last === undefined ? undefined : CLOSERS[last];
		if (last !== undefined && opener !== undefined && count(out, last) > count(out, opener)) {
			out = out.slice(0, -1);
		}
		if (out === before) {
			return out;
		}
	}
}

export function splitTextLinks(text: string): TextSegment[] {
	const segments: TextSegment[] = [];
	let last = 0;
	for (const match of text.matchAll(URL_RE)) {
		const start = match.index;
		const url = trimUrl(match[0]);
		// `https://` alone (or followed only by punctuation) isn't a link.
		if (url.length <= url.indexOf('://') + 3) {
			continue;
		}
		if (start > last) {
			segments.push({ kind: 'text', text: text.slice(last, start) });
		}
		segments.push({ kind: 'link', text: url, href: url });
		last = start + url.length;
	}
	if (last < text.length) {
		segments.push({ kind: 'text', text: text.slice(last) });
	}
	return segments;
}
