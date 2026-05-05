<script lang="ts">
	import { openExternalMarkdownLink, renderMarkdown } from '../markdown';
	import { workspace } from '../state.svelte';

	type Props = { text: string };
	let { text }: Props = $props();

	// Same async-render dance as `MarkdownView.svelte`: `renderMarkdown`
	// preloads CodeMirror grammars for every fenced-code language
	// before the synchronous render, so the call has to be awaited.
	// `stale` flips when `text` changes mid-render so the older HTML
	// is dropped on the floor — relevant once streaming arrives in
	// 6.1, harmless for the non-streaming 6.0 path.
	let html = $state('');
	$effect(() => {
		let stale = false;
		const source = text;
		void (async () => {
			const rendered = await renderMarkdown(source);
			if (!stale) {
				html = rendered;
			}
		})();
		return () => {
			stale = true;
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
-->
<!-- svelte-ignore a11y_click_events_have_key_events -->
<!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
<article class="markdown-body coder-md" onclick={onArticleClick}>
	{@html html}
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
</style>
