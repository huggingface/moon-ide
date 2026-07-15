<script lang="ts">
	import { onDestroy } from 'svelte';
	import {
		getCachedMarkdownBlocks,
		handleMarkdownCopyClick,
		openExternalMarkdownLink,
		renderMarkdownBlocks,
		type MarkdownBlock,
	} from '../markdown';
	import { workspace } from '../state.svelte';
	import { visibleOnce } from '../actions/visibleOnce';

	type Props = { text: string };
	let { text }: Props = $props();

	// Block-level streaming render: instead of one `{@html html}` that
	// rebuilds the entire markdown subtree on every chunk, we split the
	// parsed token stream into top-level blocks (paragraphs, fences,
	// lists, …) and render each independently. During streaming, only
	// the last (still-growing) block changes between deltas; all earlier
	// blocks are "frozen" and hit the per-block cache, so Svelte's keyed
	// `{@html}` effect sees the same HTML string and skips the
	// `innerHTML` write — frozen blocks' DOM nodes are never touched
	// mid-stream. That is what eliminates the flicker that a
	// whole-document `{@html}` rebuild caused.
	//
	// See `renderMarkdownBlocks` in `src/lib/markdown.ts` for the
	// token-level splitting and per-block caching. The rAF coalescer
	// + renderToken guard + visibility gate below are the same shape
	// as the old whole-string path — they now bound the block-array
	// re-render instead of the whole-document one.
	let blocks = $state<MarkdownBlock[]>([]);
	let pendingSource = '';
	let pendingFrame: number | null = null;
	let renderToken = 0;
	let renderInFlight = false;
	let mounted = true;
	let visible = $state(false);

	// Flip `mounted` false on component destruction so an in-flight
	// `renderMarkdownBlocks` promise that resolves after unmount skips
	// its `blocks` write and doesn't schedule a dangling rAF.
	onDestroy(() => {
		mounted = false;
	});

	function scheduleRender(): void {
		if (pendingFrame !== null) {
			return;
		}
		pendingFrame = requestAnimationFrame(() => {
			pendingFrame = null;
			if (renderInFlight) {
				return;
			}
			const source = pendingSource;
			const token = renderToken;
			renderInFlight = true;
			void (async () => {
				const rendered = await renderMarkdownBlocks(source, { linkify: true });
				renderInFlight = false;
				if (!mounted) {
					return;
				}
				// Same token-guard rationale as before: only write
				// `blocks` if no explicit invalidation (a sync cache
				// hit in the effect, which bumps `renderToken`)
				// happened while we were rendering. See the comment
				// in the `$effect` below.
				if (token === renderToken) {
					blocks = rendered;
				}
				if (pendingSource !== source) {
					scheduleRender();
				}
			})();
		});
	}

	$effect(() => {
		const source = text;
		// Sync cache hit: apply the block array inside the current
		// Svelte flush, no rAF, no async. Bumping `renderToken`
		// invalidates any in-flight async render so a stale resolve
		// doesn't overwrite the cached blocks we just installed.
		// Cache hits skip the visibility gate — applying a
		// precomputed array is essentially free and matching the
		// formatted layout on the first paint avoids a
		// placeholder→content swap when the row is already on-screen.
		const cached = getCachedMarkdownBlocks(source, { linkify: true });
		if (cached !== undefined) {
			pendingSource = source;
			renderToken++;
			if (pendingFrame !== null) {
				cancelAnimationFrame(pendingFrame);
				pendingFrame = null;
			}
			blocks = cached;
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
		renderToken++;
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
	Output of `renderMarkdownBlocks` has been DOMPurified upstream —
	see `src/lib/markdown.ts` for the rationale. svelte-ignore:
	clicks are delegated to anchors, the article isn't a button.

	`use:visibleOnce` flips `visible` once the row scrolls into
	(or near) the viewport. The cache fast-path in the script
	bypasses the gate by setting `blocks` synchronously; cache
	misses on off-screen rows fall through to the placeholder
	until the observer fires. Once `blocks` is non-empty we always
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
	{#if blocks.length > 0}
		{#each blocks as block (block.key)}
			{@html block.html}
		{/each}
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
