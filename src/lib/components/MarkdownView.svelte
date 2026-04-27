<script lang="ts">
	import { openUrl } from '@tauri-apps/plugin-opener';
	import { renderMarkdown } from '../markdown';
	import type { OpenFile } from '../state.svelte';

	type Props = { file: OpenFile };
	let { file }: Props = $props();

	// Render is pure of `file.text`, so `$derived` memoises across
	// re-renders that don't touch the source. Source-mode toggles back
	// and forth pay nothing for the round trip.
	const html = $derived(renderMarkdown(file.text));

	// Schemes that `opener:default` permits via `openUrl` — keep this
	// list in sync with that capability set. Anything else gets
	// swallowed: relative paths, `file://`, custom protocols. We can
	// teach the handler to open neighbouring `.md` files in a new tab
	// when somebody asks; today that's out of scope.
	const EXTERNAL_SCHEMES = new Set(['http:', 'https:', 'mailto:', 'tel:']);

	/**
	 * Anchor clicks inside the rendered article must never navigate
	 * the Tauri webview itself — that would replace the IDE shell
	 * with the page. For external schemes we hand the URL to the OS
	 * (default browser, mail client, dialer); for in-page `#anchors`
	 * we let the browser do its native scroll; everything else is
	 * dropped on the floor.
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
		// `URL` parsing is the simplest scheme check that also rejects
		// "javascript:foo" reliably (DOMPurify already blocks it, but
		// the click handler is the second line of defense).
		let url: URL;
		try {
			url = new URL(href);
		} catch {
			return;
		}
		if (!EXTERNAL_SCHEMES.has(url.protocol)) {
			return;
		}
		void openUrl(url.toString());
	}
</script>

<!--
	Anything inside `.markdown-body` came out of DOMPurify, which is
	why we trust `innerHTML`. See `src/lib/markdown.ts` for the
	rendering pipeline (markdown-it html=false → DOMPurify) and the
	XSS rationale.

	The click handler intercepts links inside the article so they
	can't navigate the Tauri webview. svelte-ignore: the click is
	delegated to anchors only — the article itself isn't a button.
-->
<div class="preview">
	<!-- svelte-ignore a11y_click_events_have_key_events -->
	<!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
	<article class="markdown-body" onclick={onArticleClick}>
		{@html html}
	</article>
</div>

<style>
	.preview {
		flex: 1;
		min-width: 0;
		min-height: 0;
		overflow: auto;
		background: var(--m-bg);
		color: var(--m-fg);
	}
	.markdown-body {
		max-width: 880px;
		margin: 0 auto;
		padding: 32px 40px 80px;
		font-family: var(--m-font-ui);
		font-size: 14px;
		line-height: 1.6;
	}
	.markdown-body :global(h1),
	.markdown-body :global(h2),
	.markdown-body :global(h3),
	.markdown-body :global(h4),
	.markdown-body :global(h5),
	.markdown-body :global(h6) {
		margin: 1.6em 0 0.6em;
		line-height: 1.25;
		font-weight: 600;
	}
	.markdown-body :global(h1):first-child,
	.markdown-body :global(h2):first-child {
		margin-top: 0;
	}
	.markdown-body :global(h1) {
		font-size: 1.8em;
		border-bottom: 1px solid var(--m-border);
		padding-bottom: 0.3em;
	}
	.markdown-body :global(h2) {
		font-size: 1.4em;
		border-bottom: 1px solid var(--m-border);
		padding-bottom: 0.25em;
	}
	.markdown-body :global(h3) {
		font-size: 1.2em;
	}
	.markdown-body :global(p),
	.markdown-body :global(ul),
	.markdown-body :global(ol),
	.markdown-body :global(blockquote),
	.markdown-body :global(table) {
		margin: 0.6em 0;
	}
	.markdown-body :global(ul),
	.markdown-body :global(ol) {
		padding-left: 1.6em;
	}
	.markdown-body :global(li + li) {
		margin-top: 0.2em;
	}
	.markdown-body :global(code) {
		font-family: var(--m-font-mono);
		font-size: 0.92em;
		background: var(--m-bg-overlay);
		padding: 0.15em 0.35em;
		border-radius: 3px;
	}
	.markdown-body :global(pre) {
		background: var(--m-bg-1);
		border: 1px solid var(--m-border);
		border-radius: 4px;
		padding: 12px 14px;
		overflow-x: auto;
		margin: 0.8em 0;
	}
	.markdown-body :global(pre code) {
		background: transparent;
		padding: 0;
	}
	.markdown-body :global(a) {
		color: var(--m-accent);
		text-decoration: none;
	}
	.markdown-body :global(a:hover) {
		text-decoration: underline;
	}
	.markdown-body :global(blockquote) {
		border-left: 3px solid var(--m-border);
		color: var(--m-fg-muted);
		padding: 0.2em 0.8em;
		margin-left: 0;
	}
	.markdown-body :global(hr) {
		border: none;
		border-top: 1px solid var(--m-border);
		margin: 1.4em 0;
	}
	.markdown-body :global(table) {
		border-collapse: collapse;
		width: 100%;
		font-size: 0.95em;
	}
	.markdown-body :global(th),
	.markdown-body :global(td) {
		border: 1px solid var(--m-border);
		padding: 6px 10px;
		text-align: left;
	}
	.markdown-body :global(th) {
		background: var(--m-bg-1);
	}
	.markdown-body :global(img) {
		max-width: 100%;
	}
</style>
