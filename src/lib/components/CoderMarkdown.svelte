<script lang="ts">
	import { openExternalMarkdownLink, renderMarkdown } from '../markdown';
	import { workspace } from '../state.svelte';

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
	let html = $state('');
	let pendingSource = '';
	let pendingFrame: number | null = null;
	let renderToken = 0;

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
		pendingSource = text;
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
