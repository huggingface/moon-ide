// Fenced-code-block highlighter for Markdown bodies (README previews,
// LSP hover popovers, any future rich-text surface).
//
// Why roll our own instead of Shiki:
//
// - We already bundle a CodeMirror grammar per supported language for
//   the live editor. Reusing those parsers means a `const x = 1` in a
//   hover popover renders with pixel-identical colors to the same code
//   in the editor — the kind of detail that makes the IDE feel
//   coherent rather than a collection of widgets.
// - Zero new dependency weight: `@lezer/highlight` and the `lang-*`
//   packages are already in the tree.
// - `classHighlighter` from `@lezer/highlight` emits stable semantic
//   class names (`tok-keyword`, `tok-string`, …) that we paint via
//   CSS. A light/dark theme flip is a `:root` variable swap — nothing
//   in here has to rerun.
//
// Trade-off: only languages we've wired into CM get syntax highlighted.
// Unknown fences fall back to plain `<pre><code>` (no highlighting, no
// error). That's the right failure mode — silently mis-highlighting a
// snippet is worse than leaving it plain.
//
// Async shape: CM language packs are lazy-loaded, so callers must
// `await loadHighlighters([...langs])` before invoking `highlightCode`
// synchronously (as `markdown-it`'s `highlight` option demands).
// `renderMarkdown` in `src/lib/markdown.ts` does this for you — most
// callers should use that instead of touching this module directly.

import { StreamLanguage } from '@codemirror/language';
import { classHighlighter, highlightTree } from '@lezer/highlight';
import type { Parser } from '@lezer/common';

/**
 * Canonical fence-language id → lazy parser factory. Keys are the
 * strings users actually type after the opening backticks in a fenced
 * code block; aliases share an entry via the normalisation table
 * below rather than being duplicated here.
 *
 * Keep this in sync with `languageFor` in `./language.ts` — every
 * language we bundle for the editor should also highlight in prose.
 * Divergence would mean "same code, different colors depending on
 * which pane it's in", which is exactly the inconsistency this
 * module exists to prevent.
 */
const LOADERS: Record<string, () => Promise<Parser>> = {
	typescript: async () => (await import('@codemirror/lang-javascript')).typescriptLanguage.parser,
	tsx: async () => (await import('@codemirror/lang-javascript')).tsxLanguage.parser,
	javascript: async () => (await import('@codemirror/lang-javascript')).javascriptLanguage.parser,
	jsx: async () => (await import('@codemirror/lang-javascript')).jsxLanguage.parser,
	json: async () => (await import('@codemirror/lang-json')).jsonLanguage.parser,
	css: async () => (await import('@codemirror/lang-css')).cssLanguage.parser,
	html: async () => (await import('@codemirror/lang-html')).htmlLanguage.parser,
	markdown: async () => (await import('@codemirror/lang-markdown')).markdownLanguage.parser,
	rust: async () => (await import('@codemirror/lang-rust')).rustLanguage.parser,
	python: async () => (await import('@codemirror/lang-python')).pythonLanguage.parser,
	toml: async () => {
		const { toml } = await import('@codemirror/legacy-modes/mode/toml');
		return StreamLanguage.define(toml).parser;
	},
	properties: async () => {
		const { properties } = await import('@codemirror/legacy-modes/mode/properties');
		return StreamLanguage.define(properties).parser;
	},
	shell: async () => {
		const { shell } = await import('@codemirror/legacy-modes/mode/shell');
		return StreamLanguage.define(shell).parser;
	},
	yaml: async () => {
		const { yaml } = await import('@codemirror/legacy-modes/mode/yaml');
		return StreamLanguage.define(yaml).parser;
	},
	dockerfile: async () => {
		const { dockerFile } = await import('@codemirror/legacy-modes/mode/dockerfile');
		return StreamLanguage.define(dockerFile).parser;
	},
};

/**
 * Fence id → canonical id used as a `LOADERS` key. Lets users write
 * `ts`, `js`, `py`, etc. without us duplicating loader entries. A
 * normalisation miss falls through to `null`, which means "plain
 * fallback" — never a silent mis-highlight.
 */
const ALIASES: Record<string, keyof typeof LOADERS> = {
	ts: 'typescript',
	mts: 'typescript',
	cts: 'typescript',
	js: 'javascript',
	mjs: 'javascript',
	cjs: 'javascript',
	jsonc: 'json',
	scss: 'css',
	less: 'css',
	htm: 'html',
	svelte: 'html',
	md: 'markdown',
	rs: 'rust',
	py: 'python',
	pyi: 'python',
	sh: 'shell',
	bash: 'shell',
	zsh: 'shell',
	yml: 'yaml',
};

/**
 * Resolve an input fence string (`ts`, `rust`, `  TOML  `, …) to the
 * canonical loader key, or `null` if we don't ship a grammar for it.
 * Case-insensitive and whitespace-tolerant because users type what
 * they type — the markdown-it fence info doesn't normalise.
 *
 * Return type is a plain `string` (a key present in `LOADERS`) —
 * `keyof typeof LOADERS` reduces to `string` because `LOADERS` is a
 * `Record<string, …>`, so the narrower typing would be compiler
 * noise without buying any additional safety.
 */
function canonicalLang(raw: string): string | null {
	const key = raw.trim().toLowerCase();
	if (!key) {
		return null;
	}
	if (key in LOADERS) {
		return key;
	}
	if (key in ALIASES) {
		const alias = ALIASES[key];
		if (alias !== undefined) {
			return alias;
		}
	}
	return null;
}

const PARSER_CACHE = new Map<string, Parser>();

/**
 * Preload parsers for every language in `rawLangs` that we know how
 * to highlight. Unknown or blank langs are skipped — they degrade to
 * plain `<pre>` at render time, no error.
 *
 * Idempotent: repeat calls reuse the cache. The common case (a hover
 * popover that's already shown a `typescript` snippet) is effectively
 * free.
 */
export async function loadHighlighters(rawLangs: readonly string[]): Promise<void> {
	const unique = new Set<string>();
	for (const raw of rawLangs) {
		const canon = canonicalLang(raw);
		if (canon !== null && !PARSER_CACHE.has(canon)) {
			unique.add(canon);
		}
	}
	if (unique.size === 0) {
		return;
	}
	await Promise.all(
		Array.from(unique).map(async (canon) => {
			// Double-check the cache inside the map fn — concurrent
			// calls for overlapping lang sets would otherwise kick off
			// duplicate dynamic imports. The dynamic import itself is
			// module-level cached by the bundler, but we still avoid
			// the extra promise hop.
			if (PARSER_CACHE.has(canon)) {
				return;
			}
			const loader = LOADERS[canon];
			if (!loader) {
				return;
			}
			try {
				const parser = await loader();
				PARSER_CACHE.set(canon, parser);
			} catch (err) {
				// A dynamic-import failure here would mean the user
				// saw plain `<pre>` instead of colored code — survivable.
				// Log so a real bundler miswire doesn't go silent.
				// eslint-disable-next-line no-console
				console.warn(`moon-ide: failed to load highlighter for "${canon}"`, err);
			}
		}),
	);
}

// Fence regex used to pre-scan a Markdown source for the languages we
// need to preload. Deliberately forgiving: matches ` ``` ` or ` ~~~ `,
// any fence length ≥ 3 at line start, optional language info string.
// Nested fences of the *same* char + length wouldn't match either way,
// and markdown-it handles them correctly at render time — we just need
// to know which loaders to warm up, not build a perfect AST.
const FENCE_RE = /^(?:\s{0,3})(```+|~~~+)[ \t]*([^\s`]+)/gm;

/**
 * Extract the language identifiers from every fenced block in
 * `source`. Used by `renderMarkdown` to preload the exact set of
 * parsers before kicking markdown-it.
 */
export function extractFenceLanguages(source: string): string[] {
	const out: string[] = [];
	// RegExp state-machine: reset lastIndex defensively in case a
	// caller held a reference across ticks. Cheap compared to the
	// actual match loop.
	FENCE_RE.lastIndex = 0;
	let match: RegExpExecArray | null;
	while ((match = FENCE_RE.exec(source)) !== null) {
		const lang = match[2];
		if (lang) {
			out.push(lang);
		}
	}
	return out;
}

/**
 * Produce an inner HTML string for a fenced code block. Returns `''`
 * when we have no grammar for `rawLang` — the caller (markdown-it's
 * `highlight` hook) interprets the empty string as "fall back to the
 * default `<pre><code>` render", which keeps the code readable and
 * selectable even without color.
 *
 * The returned HTML is the full `<pre class="cm-code"><code …>…</code></pre>`
 * wrapper; markdown-it substitutes it verbatim when `highlight` returns
 * something non-empty.
 */
export function highlightCode(code: string, rawLang: string): string {
	const canon = canonicalLang(rawLang);
	if (canon === null) {
		return '';
	}
	const parser = PARSER_CACHE.get(canon);
	if (!parser) {
		// Parser wasn't preloaded. Caller skipped `loadHighlighters`
		// or passed a source string that changed its fence set between
		// the scan and the render — either way, the right move is to
		// fall back silently. A warn log here would fire on every
		// cold-load race so it's not worth the noise.
		return '';
	}

	const tree = parser.parse(code);
	let html = '';
	let lastPos = 0;
	highlightTree(tree, classHighlighter, (from, to, classes) => {
		if (from > lastPos) {
			html += escapeHtml(code.slice(lastPos, from));
		}
		html += `<span class="${classes}">${escapeHtml(code.slice(from, to))}</span>`;
		lastPos = to;
	});
	if (lastPos < code.length) {
		html += escapeHtml(code.slice(lastPos));
	}

	return `<pre class="cm-code"><code class="language-${escapeAttr(canon)}">${html}</code></pre>`;
}

function escapeHtml(s: string): string {
	return s
		.replace(/&/g, '&amp;')
		.replace(/</g, '&lt;')
		.replace(/>/g, '&gt;')
		.replace(/"/g, '&quot;')
		.replace(/'/g, '&#39;');
}

function escapeAttr(s: string): string {
	return s.replace(/[^a-zA-Z0-9_-]/g, '');
}
