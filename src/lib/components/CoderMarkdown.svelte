<script lang="ts">
	import { getCachedMarkdown, handleMarkdownCopyClick, openExternalMarkdownLink, renderMarkdown } from '../markdown';
	import { workspace } from '../state.svelte';
	import { visibleOnce } from '../actions/visibleOnce';

	type Props = { text: string };
	let { text }: Props = $props();

	// Same async-render dance as `MarkdownView.svelte`: `renderMarkdown`
	// preloads CodeMirror grammars for every fenced-code language
	// before the synchronous render, so the call has to be awaited.
	//
	// Streaming (Phase 6.1) lands ~30 deltas/second per assistant
	// message. Re-running markdown-it + DOMPurify + grammar lookup
	// once per delta is wasteful; coalesce to one render per
	// animation frame and always pick up the *latest* source. The
	// effect itself stays cheap — only `pendingSource` mutates per
	// keystroke; the rAF callback does the actual work.
	//
	// Folder-switch fast path: if the text was rendered earlier in
	// this dev session, `getCachedMarkdown` returns the sanitised
	// HTML synchronously and we apply it during the same Svelte
	// flush as the mount. Skipping the `rAF` + async `renderMarkdown`
	// dance for cached rows is what lets a folder-swap back to an
	// already-visited session avoid the cascade of N concurrent
	// `{@html}` updates that previously batched into one big style
	// recalc (test-plan 0076, ship 6).
	//
	// Cold-cache fast path: rows that are below the fold on mount
	// stay as a plain-text placeholder until their `visibleOnce`
	// observer fires (test-plan 0076, ship 7). The placeholder
	// keeps the text searchable for `Ctrl+F` and roughly preserves
	// row height; the async render only kicks off once the user
	// scrolls close to the row, so a fresh folder swap into a long
	// session no longer pays for N concurrent grammar loads + N
	// concurrent `{@html}` swaps.
	let html = $state('');
	let pendingSource = '';
	let pendingFrame: number | null = null;
	let renderToken = 0;
	let visible = $state(false);

	function scheduleRender(): void {
		if (pendingFrame !== null) {
			return;
		}
		pendingFrame = requestAnimationFrame(() => {
			pendingFrame = null;
			const source = pendingSource;
			const token = ++renderToken;
			void (async () => {
				// `linkify: true` so raw URLs in the model's prose
				// become clickable. Differs from file-content
				// rendering (`MarkdownView.svelte`), which leaves
				// linkify off because the author would have used
				// `[text](url)` if they meant a link.
				const rendered = await renderMarkdown(source, { linkify: true });
				if (token !== renderToken) {
					return;
				}
				html = rendered;
			})();
		});
	}

	$effect(() => {
		const source = text;
		// Sync cache hit: apply rendered HTML inside the current
		// Svelte flush, no rAF, no async. Bumping `renderToken`
		// invalidates any in-flight async render so a stale resolve
		// doesn't overwrite the cached value we just installed.
		// Cache hits skip the visibility gate entirely — applying a
		// precomputed string is essentially free and matching the
		// formatted layout on the first paint avoids a
		// placeholder→content swap when the row is already on-screen.
		const cached = getCachedMarkdown(source, { linkify: true });
		if (cached !== undefined) {
			pendingSource = source;
			renderToken++;
			if (pendingFrame !== null) {
				cancelAnimationFrame(pendingFrame);
				pendingFrame = null;
			}
			html = cached;
			return () => {};
		}
		// Cache miss: defer the async render until the row is in
		// (or near) the viewport. Off-screen rows stay as the
		// plain-text placeholder; the `visibleOnce` action flips
		// `visible` on first intersection and the effect re-runs
		// to schedule the render.
		if (!visible) {
			return () => {};
		}
		pendingSource = source;
		scheduleRender();
		return () => {
			if (pendingFrame !== null) {
				cancelAnimationFrame(pendingFrame);
				pendingFrame = null;
			}
		};
	});

	/**
	 * Anchor clicks inside an assistant message must never navigate
	 * the Tauri webview itself — that would replace the IDE shell
	 * with whatever the agent linked. Routing:
	 *   - external schemes (`http(s)://`, `mailto:`, `tel:`) → OS
	 *     default app via `openExternalMarkdownLink`
	 *   - in-page `#anchors` → native browser scroll
	 *   - everything else (relative or `/`-rooted paths) is treated
	 *     as a workspace-root-relative file path and handed to
	 *     `workspace.openFile`. Differs from `MarkdownView`'s "resolve
	 *     against the document's directory" because a chat reply has
	 *     no document; the model nearly always names files
	 *     workspace-relative anyway (`src/foo.rs`).
	 */
	function onArticleClick(event: MouseEvent) {
		const target = event.target;
		if (!(target instanceof HTMLElement)) {
			return;
		}
		if (handleMarkdownCopyClick(event)) {
			return;
		}
		const anchor = target.closest('a');
		if (!anchor) {
			return;
		}
		const href = anchor.getAttribute('href');
		if (!href) {
			event.preventDefault();
			return;
		}
		if (href.startsWith('#')) {
			return;
		}
		event.preventDefault();
		if (openExternalMarkdownLink(href)) {
			return;
		}
		const stripped = href.replace(/^\/+/, '').split('?')[0]?.split('#')[0] ?? '';
		if (!stripped) {
			return;
		}
		void workspace.openFile(stripped);
	}
</script>

<!--
	Output of `renderMarkdown` has been DOMPurified upstream — see
	`src/lib/markdown.ts` for the rationale. svelte-ignore: clicks
	are delegated to anchors, the article isn't a button.

	`use:visibleOnce` flips `visible` once the row scrolls into
	(or near) the viewport. The cache fast-path in the script
	bypasses the gate by setting `html` synchronously; cache
	misses on off-screen rows fall through to the placeholder
	until the observer fires. Once `html` is non-empty we always
	render the formatted output — the placeholder is purely a
	pre-render state, never a "scrolled away" state.
-->
<!-- svelte-ignore a11y_click_events_have_key_events -->
<!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
<article
	class="markdown-body coder-md"
	onclick={onArticleClick}
	use:visibleOnce={() => {
		visible = true;
	}}
>
	{#if html !== ''}
		{@html html}
	{:else}
		<span class="coder-md-placeholder">{text}</span>
	{/if}
</article>

<style>
	/* The shared `.markdown-body` rules in `src/styles.css` give us
	   typography, code-block colors, and link styling. The overrides
	   below trim block-margins so a single-paragraph reply doesn't
	   stack big top/bottom gaps inside the chat bubble — that
	   margin is the "empty space at the beginning" effect we want
	   to kill. */
	.coder-md :global(> :first-child) {
		margin-top: 0;
	}
	.coder-md :global(> :last-child) {
		margin-bottom: 0;
	}
	.coder-md :global(p),
	.coder-md :global(ul),
	.coder-md :global(ol),
	.coder-md :global(blockquote),
	.coder-md :global(pre),
	.coder-md :global(table) {
		margin: 6px 0;
	}
	.coder-md :global(h1),
	.coder-md :global(h2),
	.coder-md :global(h3),
	.coder-md :global(h4),
	.coder-md :global(h5),
	.coder-md :global(h6) {
		margin: 10px 0 4px;
	}
	.coder-md :global(pre) {
		max-width: 100%;
		overflow: auto;
	}
	/* Plain-text placeholder shown while a cache-miss row is still
	   off-screen. Keeps `Ctrl+F` working by leaving the raw text in
	   the DOM and roughly preserves row height: chat messages are
	   mostly prose, so the wrapped text approximates the formatted
	   paragraph layout closely enough that the placeholder→content
	   swap doesn't shift the surrounding rows on most scrolls.
	   Fenced code blocks render as wrapped plain text in this state
	   (no monospace fallback) — acceptable for the few frames the
	   placeholder is on screen. */
	.coder-md-placeholder {
		display: block;
		white-space: pre-wrap;
		word-break: break-word;
	}
</style>
