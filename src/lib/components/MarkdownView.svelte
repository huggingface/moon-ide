<script lang="ts">
	import { openUrl } from '@tauri-apps/plugin-opener';
	import { renderMarkdown, resolveMarkdownLink } from '../markdown';
	import { isUntitledPath, workspace, type OpenFile } from '../state.svelte';

	type Props = { file: OpenFile };
	let { file }: Props = $props();

	// `renderMarkdown` is async because it pre-loads the CM grammar for
	// every fenced-code language in the source before handing to
	// markdown-it (dynamic imports can't happen mid-render). The
	// effect races naturally: if `file.text` changes before the
	// previous render resolves, `stale` flips and the older result is
	// dropped on the floor. A `$derived` can't express this.
	let html = $state('');
	$effect(() => {
		let stale = false;
		const source = file.text;
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

	// Schemes that `opener:default` permits via `openUrl` — keep this
	// list in sync with that capability set. Anything else with a
	// recognised scheme is dropped on the floor (file://, custom
	// protocols, etc.).
	const EXTERNAL_SCHEMES = new Set(['http:', 'https:', 'mailto:', 'tel:']);

	/**
	 * Anchor clicks inside the rendered article must never navigate
	 * the Tauri webview itself — that would replace the IDE shell
	 * with the page. The handler routes by link shape:
	 *   - external schemes (`http(s)://`, `mailto:`, `tel:`) → OS
	 *     default app via `openUrl`
	 *   - in-page `#anchors` → native browser scroll
	 *   - relative or workspace-root-absolute paths → open the target
	 *     file in a new tab inside the IDE
	 *   - everything else → swallow
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
		// `URL` only parses absolute URLs; relative paths throw, which
		// is how we tell them apart. Going through `URL` also rejects
		// `javascript:foo` reliably (DOMPurify already blocks it, but
		// this is the second line of defence).
		let absolute: URL | null = null;
		try {
			absolute = new URL(href);
		} catch {
			absolute = null;
		}
		if (absolute) {
			if (EXTERNAL_SCHEMES.has(absolute.protocol)) {
				void openUrl(absolute.toString());
			}
			return;
		}
		// Relative link: resolve it against the current file's location
		// and ask WorkspaceState to open it. Untitled buffers have no
		// on-disk directory to resolve against — we'd never preview one
		// in practice (preview requires a markdown extension), but be
		// defensive in case that invariant slips later.
		if (isUntitledPath(file.path)) {
			return;
		}
		const targetPath = resolveMarkdownLink(file.path, href);
		if (!targetPath) {
			return;
		}
		void workspace.openFile(targetPath);
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
	/* Most `.markdown-body` rules live in `src/styles.css` so the LSP
	   hover popover inherits the same look — MarkdownView keeps only
	   the layout-specific bits (scrolling wrapper, reading-width
	   column, generous padding) that belong to this view. */
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
		font-size: 14px;
	}
</style>
