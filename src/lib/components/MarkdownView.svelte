<script lang="ts">
	import { tick } from 'svelte';
	import { handleMarkdownCopyClick, openExternalMarkdownLink, renderMarkdown, resolveMarkdownLink } from '../markdown';
	import { isUntitledPath, workspace, type OpenFile, type SplitSide } from '../state.svelte';

	type Props = { file: OpenFile; side: SplitSide };
	let { file, side }: Props = $props();

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

	// Find bar state. The bar piggy-backs on the CSS Custom Highlight
	// API (`CSS.highlights`) to paint matches without touching the
	// rendered article's DOM — DOMPurify gave us trusted HTML and we
	// don't want to wrap arbitrary substrings in <mark> spans across
	// nested inline tags. Highlights are purely visual; nothing about
	// the underlying markup or copy-paste changes. The current match
	// gets a brighter colour via a second registered highlight.
	let articleEl = $state<HTMLElement | null>(null);
	let inputEl = $state<HTMLInputElement | null>(null);
	let findOpen = $state(false);
	let query = $state('');
	let matchIndex = $state(0);
	let matchCount = $state(0);
	const FIND_HL = 'moon-md-find';
	const FIND_HL_ACTIVE = 'moon-md-find-active';
	// Keep the Range list around so prev/next can re-target the
	// active highlight without recomputing all matches.
	let ranges: Range[] = [];

	// Only the focused pane reacts to Ctrl+F. Without this the other
	// split (also showing markdown preview) would race to open its
	// own bar and steal focus.
	function onWindowKeydown(event: KeyboardEvent) {
		if (workspace.focusedSide !== side) {
			return;
		}
		const ctrl = event.ctrlKey || event.metaKey;
		if (ctrl && !event.shiftKey && !event.altKey && event.key.toLowerCase() === 'f') {
			event.preventDefault();
			openFind();
		}
	}

	function openFind() {
		findOpen = true;
		const selected = window.getSelection()?.toString() ?? '';
		if (selected && articleEl?.contains(window.getSelection()?.anchorNode ?? null)) {
			query = selected;
		}
		// Wait for the input to mount, then focus + select so the
		// user can immediately retype or hit Enter.
		void tick().then(() => {
			inputEl?.focus();
			inputEl?.select();
		});
	}

	function closeFind() {
		findOpen = false;
		query = '';
		matchIndex = 0;
		matchCount = 0;
		ranges = [];
		clearHighlights();
	}

	function clearHighlights() {
		if (typeof CSS === 'undefined' || !('highlights' in CSS)) {
			return;
		}
		CSS.highlights.delete(FIND_HL);
		CSS.highlights.delete(FIND_HL_ACTIVE);
	}

	// Walk the article's text nodes and collect Ranges for every
	// case-insensitive occurrence of `needle`. We deliberately match
	// inside a single text node at a time — markdown-it produces
	// short text nodes split by inline tags, and matching across
	// node boundaries would mean either flattening the DOM (loses
	// styling) or sliding a buffer (complex, rarely needed for
	// prose). The trade-off: a query spanning a `<code>` boundary
	// won't match. Acceptable for a docs-style find.
	function computeMatches() {
		ranges = [];
		matchIndex = 0;
		matchCount = 0;
		clearHighlights();
		const root = articleEl;
		if (!root || query === '') {
			return;
		}
		if (typeof CSS === 'undefined' || !('highlights' in CSS) || typeof Highlight === 'undefined') {
			return;
		}
		const needle = query.toLowerCase();
		const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT, {
			acceptNode(node) {
				// Skip empty text and whitespace-only nodes early —
				// they can't contain matches and walking them just
				// burns time on long articles.
				if (!node.nodeValue || node.nodeValue.trim() === '') {
					return NodeFilter.FILTER_REJECT;
				}
				return NodeFilter.FILTER_ACCEPT;
			},
		});
		let current = walker.nextNode();
		while (current) {
			const text = current.nodeValue ?? '';
			const lower = text.toLowerCase();
			let from = 0;
			while (from <= lower.length - needle.length) {
				const hit = lower.indexOf(needle, from);
				if (hit === -1) {
					break;
				}
				const range = document.createRange();
				range.setStart(current, hit);
				range.setEnd(current, hit + needle.length);
				ranges.push(range);
				from = hit + needle.length;
			}
			current = walker.nextNode();
		}
		matchCount = ranges.length;
		matchIndex = matchCount === 0 ? 0 : 1;
		paintHighlights();
		scrollActiveIntoView();
	}

	function paintHighlights() {
		if (typeof CSS === 'undefined' || !('highlights' in CSS) || typeof Highlight === 'undefined') {
			return;
		}
		if (ranges.length === 0) {
			clearHighlights();
			return;
		}
		const active = matchIndex > 0 ? ranges[matchIndex - 1] : null;
		const rest = active ? ranges.filter((_, i) => i !== matchIndex - 1) : ranges;
		CSS.highlights.set(FIND_HL, new Highlight(...rest));
		if (active) {
			CSS.highlights.set(FIND_HL_ACTIVE, new Highlight(active));
		} else {
			CSS.highlights.delete(FIND_HL_ACTIVE);
		}
	}

	function scrollActiveIntoView() {
		if (matchIndex === 0) {
			return;
		}
		const range = ranges[matchIndex - 1];
		if (!range) {
			return;
		}
		// Anchor the scroll on the match's parent element — `Range`
		// doesn't have a `scrollIntoView`, and grabbing the rect
		// directly would only work for in-flow text.
		const node = range.startContainer.parentElement;
		node?.scrollIntoView({ block: 'center', behavior: 'smooth' });
	}

	function step(delta: number) {
		if (matchCount === 0) {
			return;
		}
		// Wrap around in both directions so the user can spin
		// forward off the end and land back on the first hit.
		matchIndex = ((matchIndex - 1 + delta + matchCount) % matchCount) + 1;
		paintHighlights();
		scrollActiveIntoView();
	}

	function onFindKeydown(event: KeyboardEvent) {
		if (event.key === 'Escape') {
			event.preventDefault();
			closeFind();
			return;
		}
		if (event.key === 'Enter') {
			event.preventDefault();
			if (matchCount === 0) {
				return;
			}
			step(event.shiftKey ? -1 : 1);
			return;
		}
	}

	// Re-compute on query change (live) and on html change (the
	// markdown re-rendered under us — e.g. file save). The
	// dependency on `html` keeps the highlight set in sync with the
	// DOM; without it the saved ranges would point at detached
	// nodes and the highlight API would silently render nothing.
	$effect(() => {
		void query;
		void html;
		if (!findOpen) {
			return;
		}
		// Wait one tick so the new `{@html html}` is in the DOM
		// before we walk it.
		void tick().then(computeMatches);
	});

	// When the file path changes (tab swap) the parent re-keys this
	// component, so we don't need an explicit reset there. But a
	// late-arriving render of the previous file could still flash —
	// clearing on unmount keeps `CSS.highlights` from leaking
	// across surfaces.
	$effect(() => {
		return () => {
			clearHighlights();
		};
	});

	/**
	 * Anchor clicks inside the rendered article must never navigate
	 * the Tauri webview itself — that would replace the IDE shell
	 * with the page. The handler routes by link shape:
	 *   - external schemes (`http(s)://`, `mailto:`, `tel:`) → OS
	 *     default app via `openExternalMarkdownLink`
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
			// Resolve in-page anchors against this article rather
			// than letting the browser do it: native fragment scroll
			// also updates `location.hash`, which under Tauri can
			// fire navigation listeners and ends up as junk in the
			// session-restore URL. `scrollIntoView` with `smooth`
			// gives a nicer ride too. Falls back to the browser's
			// default scroll if we can't find the target (e.g. a
			// stale link to a since-renamed heading) — that lets
			// the user see in the URL what they tried to hit.
			const id = decodeURIComponent(href.slice(1));
			const dest = id ? articleEl?.querySelector(`[id="${CSS.escape(id)}"]`) : null;
			if (dest) {
				event.preventDefault();
				dest.scrollIntoView({ behavior: 'smooth', block: 'start' });
			}
			return;
		}
		event.preventDefault();
		if (openExternalMarkdownLink(href)) {
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

<svelte:window onkeydown={onWindowKeydown} />

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
	{#if findOpen}
		<!-- The find bar floats over the article in the top-right
		     corner. Sticky positioning would jump the layout on
		     open/close; absolute keeps the column width steady. -->
		<div class="find-bar" role="search">
			<input
				bind:this={inputEl}
				bind:value={query}
				type="text"
				placeholder="Find in document"
				aria-label="Find in document"
				spellcheck="false"
				onkeydown={onFindKeydown}
			/>
			<span class="count" aria-live="polite">
				{#if query === ''}
					&nbsp;
				{:else if matchCount === 0}
					No results
				{:else}
					{matchIndex} / {matchCount}
				{/if}
			</span>
			<button
				type="button"
				class="step"
				aria-label="Previous match"
				title="Previous (Shift+Enter)"
				onclick={() => step(-1)}
				disabled={matchCount === 0}
			>
				&#8593;
			</button>
			<button
				type="button"
				class="step"
				aria-label="Next match"
				title="Next (Enter)"
				onclick={() => step(1)}
				disabled={matchCount === 0}
			>
				&#8595;
			</button>
			<button type="button" class="close" aria-label="Close find" title="Close (Esc)" onclick={closeFind}>
				&#215;
			</button>
		</div>
	{/if}
	<!-- svelte-ignore a11y_click_events_have_key_events -->
	<!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
	<article bind:this={articleEl} class="markdown-body" onclick={onArticleClick}>
		{@html html}
	</article>
</div>

<style>
	/* Most `.markdown-body` rules live in `src/styles.css` so the LSP
	   hover popover inherits the same look — MarkdownView keeps only
	   the layout-specific bits (scrolling wrapper, reading-width
	   column, generous padding) that belong to this view. */
	.preview {
		position: relative;
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

	.find-bar {
		position: sticky;
		top: 8px;
		margin: 8px 8px 0 auto;
		width: fit-content;
		display: flex;
		align-items: center;
		gap: 6px;
		padding: 6px 8px;
		background: var(--m-bg-2);
		border: 1px solid var(--m-border-strong);
		border-radius: 6px;
		box-shadow: 0 4px 12px rgba(0, 0, 0, 0.35);
		font-size: 12px;
		/* Float over the article without consuming its layout
		   space — the sticky top keeps it pinned while scrolling. */
		z-index: 2;
	}
	.find-bar input {
		width: 220px;
		padding: 4px 6px;
		background: var(--m-bg-1);
		color: var(--m-fg);
		border: 1px solid var(--m-border);
		border-radius: 4px;
		font: inherit;
		outline: none;
	}
	.find-bar input:focus {
		border-color: var(--m-accent);
	}
	.find-bar .count {
		min-width: 64px;
		text-align: center;
		color: var(--m-fg);
		opacity: 0.75;
		font-variant-numeric: tabular-nums;
	}
	.find-bar button {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		width: 22px;
		height: 22px;
		padding: 0;
		background: transparent;
		color: var(--m-fg);
		border: 1px solid transparent;
		border-radius: 4px;
		cursor: pointer;
		font-size: 13px;
		line-height: 1;
	}
	.find-bar button:hover:not(:disabled) {
		background: var(--m-bg-1);
		border-color: var(--m-border);
	}
	.find-bar button:disabled {
		opacity: 0.4;
		cursor: default;
	}
	.find-bar .close {
		font-size: 16px;
	}
</style>
