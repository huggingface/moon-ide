<script lang="ts">
	import { onMount } from 'svelte';
	import { Compartment, EditorState, type Extension } from '@codemirror/state';
	import { EditorView, highlightActiveLine, lineNumbers } from '@codemirror/view';
	import { bracketMatching, foldGutter } from '@codemirror/language';
	import { highlightSelectionMatches } from '@codemirror/search';
	import { MergeView, diff as rawDiff } from '@codemirror/merge';
	import { ipc } from '../ipc';
	import { workspace } from '../state.svelte';
	import { highlightTabs } from '../editor/highlightTabs';
	import { languageFor } from '../editor/language';
	import { moonEditorTheme } from '../editor/theme';
	import { diffPureChangeExtension } from '../editor/diffPureChange';
	import { diffGutterTintExtension } from '../editor/diffGutterTint';
	import type { GitFileStatus } from '../protocol';

	type Props = {
		path: string;
		status: GitFileStatus;
		// Merge-base SHA when reviewing against the default branch;
		// `null` when the active baseline is `head`, in which case
		// `loadBase` reads `git show HEAD:<path>` via
		// `gitHeadContent` instead. The section is keyed off this
		// value in the parent so a baseline flip remounts and
		// rebuilds against the right "before" content.
		mergeBase: string | null;
		// Eager: build the MergeView immediately at mount. Lazy: wait
		// for the first IntersectionObserver hit. The parent passes
		// `eager` for the first 1-2 sections (so the user sees content
		// without scrolling); everything else trickles in as the
		// reader scrolls down. Mounting all N MergeViews up-front on
		// a 100-file PR is the original "this is slow" complaint.
		eager: boolean;
		// Wired by `ReviewView` so its scroll handler can find sections
		// by path. Bound, not a callback, because Svelte 5's `bindable`
		// would be over-engineered for a single ref reach-through.
		registerSection: (path: string, el: HTMLElement | null) => void;
	};

	let { path, status, mergeBase, eager, registerSection }: Props = $props();

	let sectionEl: HTMLElement | undefined = $state();
	let host: HTMLDivElement | undefined = $state();
	let merge: MergeView | undefined = $state();
	let collapsed = $state(false);
	let mounted = $state(false);
	let loading = $state(false);
	let buildToken = 0;
	// Cleanup for the per-section horizontal scroll mirror set
	// up at the tail of `build`. Stored at component scope so the
	// `onMount` teardown (and the rebuild branch, if ever) can
	// drop the DOM listeners without leaking across remounts.
	let detachHScrollSync: (() => void) | null = null;

	// Theme + language compartments mirror DiffView's pattern so a
	// theme toggle or language hot-swap reconfigures the live merge
	// view instead of forcing a full rebuild. No editorconfig
	// compartment: review sections are read-only, default indent
	// settings don't matter visually.
	const langA = new Compartment();
	const langB = new Compartment();
	const themeA = new Compartment();
	const themeB = new Compartment();
	const wrapA = new Compartment();
	const wrapB = new Compartment();

	onMount(() => {
		registerSection(path, sectionEl ?? null);
		if (eager) {
			void build();
		} else {
			// One-shot IO: mount on first visibility. We never
			// destroy the MergeView once mounted — losing the
			// scroll/fold state on scroll-away would be confusing,
			// and the memory cost of an already-built CM6 editor
			// off-screen is small compared to the diff bytes it
			// already paid to compute.
			if (sectionEl) {
				const io = new IntersectionObserver(
					(entries) => {
						for (const entry of entries) {
							if (entry.isIntersecting) {
								io.disconnect();
								void build();
								return;
							}
						}
					},
					// Pre-build ~half a viewport early so the section is
					// painted by the time the user scrolls to it.
					{ rootMargin: '50% 0px' },
				);
				io.observe(sectionEl);
				return () => {
					io.disconnect();
					registerSection(path, null);
					buildToken++;
					detachHScrollSync?.();
					detachHScrollSync = null;
					merge?.destroy();
					merge = undefined;
					clearOurSelection();
				};
			}
		}
		return () => {
			registerSection(path, null);
			buildToken++;
			detachHScrollSync?.();
			detachHScrollSync = null;
			merge?.destroy();
			merge = undefined;
			clearOurSelection();
		};
	});

	// Drop the workspace selection snapshot only when it belongs to
	// *our* section's path. Symmetric with the empty-selection
	// branch in `publishReviewSelection` — keeps a sibling section's
	// selection alive when this one unmounts (e.g. lazy mount churn).
	function clearOurSelection() {
		const current = workspace.activeSelection;
		if (current !== null && current.path === path) {
			workspace.setActiveSelection(null);
		}
	}

	// Theme follows the workspace toggle without rebuilding state.
	$effect(() => {
		const theme = workspace.effectiveTheme;
		if (!merge) {
			return;
		}
		merge.a.dispatch({ effects: themeA.reconfigure(moonEditorTheme(theme)) });
		merge.b.dispatch({ effects: themeB.reconfigure(moonEditorTheme(theme)) });
	});

	// Soft-wrap toggle mirrors the regular editor / DiffView. Both
	// sides flip together so the merge alignment stays sane.
	$effect(() => {
		const wrap = workspace.lineWrap;
		if (!merge) {
			return;
		}
		const ext = wrap ? EditorView.lineWrapping : [];
		merge.a.dispatch({ effects: wrapA.reconfigure(ext) });
		merge.b.dispatch({ effects: wrapB.reconfigure(ext) });
	});

	async function build() {
		if (mounted || loading || !host) {
			return;
		}
		loading = true;
		const token = ++buildToken;
		const [base, working] = await Promise.all([loadBase(), loadWorking()]);
		if (token !== buildToken || !host) {
			return;
		}
		const firstLine = working.length > 0 ? firstLineOf(working) : firstLineOf(base);
		const lang = await languageFor(path, firstLine);
		if (token !== buildToken || !host) {
			return;
		}

		const sharedReadOnly: Extension[] = [EditorState.readOnly.of(true), EditorView.editable.of(false)];

		const sideA: Extension[] = [
			lineNumbers(),
			diffGutterTintExtension('a'),
			foldGutter(),
			diffPureChangeExtension,
			highlightSelectionMatches(),
			highlightTabs(),
			bracketMatching(),
			themeA.of(moonEditorTheme(workspace.effectiveTheme)),
			langA.of(lang),
			wrapA.of(workspace.lineWrap ? EditorView.lineWrapping : []),
			...sharedReadOnly,
		];
		const sideB: Extension[] = [
			lineNumbers(),
			diffGutterTintExtension('b'),
			foldGutter(),
			diffPureChangeExtension,
			highlightActiveLine(),
			highlightSelectionMatches(),
			highlightTabs(),
			bracketMatching(),
			themeB.of(moonEditorTheme(workspace.effectiveTheme)),
			langB.of(lang),
			wrapB.of(workspace.lineWrap ? EditorView.lineWrapping : []),
			// Selection-publish hook on the working-tree side so
			// `Ctrl+L` can attach a highlighted span from the review
			// view to the coder, the same way it does from a regular
			// editor or the right pane of `DiffView`. We only wire
			// the right side: the left is the base/HEAD snapshot,
			// whose line numbers wouldn't match the file on disk and
			// would mislead the model. Status `deleted` has an empty
			// right side, so selection events never fire there.
			EditorView.updateListener.of((update) => {
				if (update.selectionSet) {
					publishReviewSelection(update.state);
				}
			}),
			...sharedReadOnly,
		];

		detachHScrollSync?.();
		detachHScrollSync = null;

		merge = new MergeView({
			a: { doc: base, extensions: sideA },
			b: { doc: working, extensions: sideB },
			parent: host,
			// Built-in change-bar gutter replaced by line-number
			// cell tinting via `diffGutterTintExtension` — see
			// `DiffView.svelte` for the same call site and
			// rationale.
			gutter: false,
			highlightChanges: true,
			// No revert controls — the review tab is read-only;
			// the user can't apply A→B or vice versa from here.
			//
			// Aggregated review view collapses long unchanged
			// regions behind `… N unchanged lines` placeholders.
			// In `DiffView` (single-file, one-tab-at-a-time) we
			// expanded everything because the change-bar gutter +
			// overview ruler already tell you "where the diffs
			// are" and the placeholders get in the way of Ctrl+F /
			// scroll. Here it's the opposite trade-off: on a
			// 30-file PR with a 2000-line file that only changed
			// 20 lines, an uncollapsed mount stalls the scroll
			// and forces the user to skim past acres of unchanged
			// code to reach the next file. `margin: 3` keeps
			// hunks readable (same as `git diff -U3`); `minSize:
			// 5` only folds runs of ≥5 unchanged lines so tiny
			// gaps between adjacent hunks stay expanded.
			collapseUnchanged: { margin: 3, minSize: 5 },
			diffConfig: {
				// See DiffView.svelte for the rationale: raw diff
				// keeps highlights aligned to single change spans
				// rather than fusing across short matched substrings.
				override: rawDiff,
			},
		});

		detachHScrollSync = wireHorizontalScrollSync(merge.a.scrollDOM, merge.b.scrollDOM);

		mounted = true;
		loading = false;
	}

	/**
	 * Bidirectional `scrollLeft` mirror between the two `.cm-scroller`
	 * elements of the section's MergeView. Same pattern as
	 * `DiffView.svelte`'s `wireHorizontalScrollSync`: when long
	 * lines force one side into horizontal overflow, dragging
	 * either side's bar (or wheel-scrolling horizontally) drags
	 * the other in lockstep so the aligned chunks line up
	 * visually. A `syncing` flag plus `requestAnimationFrame`
	 * release breaks the echo loop (browser dispatches the
	 * mirrored scroll event on the next frame, so a microtask
	 * would clear the guard too early). Returns a cleanup that
	 * the section's onMount teardown invokes on unmount.
	 */
	function wireHorizontalScrollSync(a: HTMLElement, b: HTMLElement): () => void {
		let syncing = false;
		const mirror = (from: HTMLElement, to: HTMLElement) => {
			if (syncing) {
				return;
			}
			if (to.scrollLeft === from.scrollLeft) {
				return;
			}
			syncing = true;
			to.scrollLeft = from.scrollLeft;
			requestAnimationFrame(() => {
				syncing = false;
			});
		};
		const onA = () => mirror(a, b);
		const onB = () => mirror(b, a);
		a.addEventListener('scroll', onA, { passive: true });
		b.addEventListener('scroll', onB, { passive: true });
		return () => {
			a.removeEventListener('scroll', onA);
			b.removeEventListener('scroll', onB);
		};
	}

	async function loadBase(): Promise<string> {
		// `added` rows have no merge-base blob; an untracked file by
		// definition isn't in git yet either. Skip the fetch — left
		// side renders empty so the diff reads as a pure addition.
		if (status === 'added' || status === 'untracked') {
			return '';
		}
		try {
			// `mergeBase === null` is the "vs HEAD" baseline (the
			// review tab opened against the working tree without
			// flipping to default-branch mode). Read the file at
			// HEAD instead of at the merge-base SHA.
			const content =
				mergeBase !== null ? await ipc.fs.gitRefContent(mergeBase, path) : await ipc.fs.gitHeadContent(path);
			return content ?? '';
		} catch {
			return '';
		}
	}

	async function loadWorking(): Promise<string> {
		// Deleted in the working tree → no bytes to show on the right.
		if (status === 'deleted') {
			return '';
		}
		// Prefer the open buffer when one exists. Reflects unsaved
		// edits in the review (matches the working-tree intent: "what
		// would I be committing right now?"). Falls back to a fresh
		// read otherwise.
		const open = workspace.openFiles.find((f) => f.path === path);
		if (open && open.kind === 'text' && !open.isDeleted) {
			return open.text;
		}
		try {
			const result = await ipc.fs.readFile(path);
			if (result.text === null) {
				return '';
			}
			return result.text;
		} catch {
			return '';
		}
	}

	function firstLineOf(text: string): string {
		const idx = text.indexOf('\n');
		return idx === -1 ? text : text.slice(0, idx);
	}

	function fileName(p: string): string {
		const slash = p.lastIndexOf('/');
		return slash === -1 ? p : p.slice(slash + 1);
	}

	function dirName(p: string): string {
		const slash = p.lastIndexOf('/');
		return slash === -1 ? '' : p.slice(0, slash);
	}

	function statusLabel(s: GitFileStatus): string {
		switch (s) {
			case 'added':
				return 'A';
			case 'modified':
				return 'M';
			case 'deleted':
				return 'D';
			case 'untracked':
				return 'U';
			default:
				return '';
		}
	}

	function openInEditor() {
		// Deleted rows can't open as a normal editor tab (there's
		// nothing to edit). Skip the action — the section already
		// shows the HEAD content on the left.
		if (status === 'deleted') {
			return;
		}
		void workspace.openFile(path);
	}

	function toggleCollapsed() {
		collapsed = !collapsed;
	}

	/**
	 * Mirror of `Editor.svelte`'s `publishSelection` and
	 * `DiffView.svelte`'s `publishDiffSelection` for the review
	 * section's working-tree pane. Empty selections clear the
	 * snapshot so `Ctrl+L` doesn't attach a stale highlight from
	 * a section the user moved away from. Same off-by-one snap
	 * when the drag ends at the start of a line the user didn't
	 * actually mean to include.
	 */
	function publishReviewSelection(state: EditorState) {
		const sel = state.selection.main;
		if (sel.empty) {
			// Only clear if the workspace snapshot is currently ours —
			// otherwise we'd stomp on a selection another section just
			// published. Side-by-side MergeViews don't share focus, but
			// CodeMirror still dispatches `selectionSet` updates when a
			// view loses focus and resets its selection.
			const current = workspace.activeSelection;
			if (current !== null && current.path === path) {
				workspace.setActiveSelection(null);
			}
			return;
		}
		const fromLine = state.doc.lineAt(sel.from);
		const toLine = state.doc.lineAt(sel.to);
		const effectiveToLineNumber =
			sel.to === toLine.from && toLine.number > fromLine.number ? toLine.number - 1 : toLine.number;
		const text = state.doc.sliceString(sel.from, sel.to);
		workspace.setActiveSelection({
			path,
			startLine: fromLine.number,
			endLine: effectiveToLineNumber,
			text,
		});
	}
</script>

<section
	bind:this={sectionEl}
	class="review-section"
	class:collapsed
	data-review-path={path}
	aria-label={`Diff of ${path}`}
>
	<header class="hdr">
		<button
			type="button"
			class="caret"
			aria-expanded={!collapsed}
			aria-label={collapsed ? 'Expand diff' : 'Collapse diff'}
			onclick={toggleCollapsed}
		>
			<span aria-hidden="true">{collapsed ? '▸' : '▾'}</span>
		</button>
		<span class="status status-{status}" title={`Status: ${status}`} aria-label={`Status ${status}`}>
			{statusLabel(status)}
		</span>
		<button type="button" class="path" title={`Open ${path}`} onclick={openInEditor}>
			{#if dirName(path)}<span class="dir">{dirName(path)}/</span>{/if}<span class="name">{fileName(path)}</span>
		</button>
	</header>
	{#if !collapsed}
		<div class="body" bind:this={host}></div>
		{#if !mounted && loading}
			<div class="placeholder">Loading diff…</div>
		{:else if !mounted}
			<div class="placeholder">Scroll to load diff</div>
		{/if}
	{/if}
</section>

<style>
	.review-section {
		display: flex;
		flex-direction: column;
		border: 1px solid var(--m-border);
		border-radius: 6px;
		background: var(--m-bg);
		overflow: hidden;
		scroll-margin-top: 12px;
	}
	.hdr {
		display: flex;
		align-items: center;
		gap: 8px;
		padding: 6px 10px;
		background: var(--m-bg-1);
		border-bottom: 1px solid var(--m-border);
		position: sticky;
		top: 0;
		z-index: 2;
	}
	.review-section.collapsed .hdr {
		border-bottom: none;
	}
	.caret {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		width: 18px;
		height: 18px;
		padding: 0;
		background: transparent;
		border: none;
		color: var(--m-fg-muted);
		cursor: pointer;
		font-size: 12px;
		line-height: 1;
	}
	.caret:hover {
		color: var(--m-fg);
	}
	.status {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		min-width: 16px;
		height: 16px;
		padding: 0 4px;
		font-size: 10px;
		font-weight: 600;
		border-radius: 3px;
		font-family: var(--m-font-mono, monospace);
	}
	.status-added {
		color: var(--m-git-added, #4ec9b0);
		background: color-mix(in srgb, var(--m-git-added, #4ec9b0) 18%, transparent);
	}
	.status-modified {
		color: var(--m-git-modified, #e2c08d);
		background: color-mix(in srgb, var(--m-git-modified, #e2c08d) 18%, transparent);
	}
	.status-deleted {
		color: var(--m-git-deleted, #f48771);
		background: color-mix(in srgb, var(--m-git-deleted, #f48771) 18%, transparent);
	}
	.status-untracked {
		color: var(--m-fg-muted);
		background: color-mix(in srgb, var(--m-fg-muted) 18%, transparent);
	}
	.path {
		flex: 1;
		min-width: 0;
		display: inline-flex;
		align-items: baseline;
		gap: 0;
		padding: 2px 4px;
		background: transparent;
		border: none;
		color: var(--m-fg);
		font-family: var(--m-font-mono, monospace);
		font-size: 12px;
		text-align: left;
		cursor: pointer;
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}
	.path:hover .name {
		text-decoration: underline;
	}
	.dir {
		color: var(--m-fg-muted);
	}
	.name {
		color: var(--m-fg);
	}
	.body {
		display: flex;
		min-height: 0;
	}
	.placeholder {
		padding: 12px;
		color: var(--m-fg-muted);
		font-size: 12px;
		font-style: italic;
		text-align: center;
	}
	/* `@codemirror/merge` ships `.cm-mergeView` as the scrolling
	 * container (`overflow-y: auto`). Inside the stacked review
	 * the *outer* `.review-view` is the scroller; relax the merge
	 * package's overflow so each section grows to its content
	 * height and the user gets one long page of diffs instead of
	 * a nested scrollbar per file. Two layout side-effects to
	 * keep an eye on:
	 *
	 *   1. The horizontal scrollbar that DiffView re-parents to a
	 *      sticky strip — we don't bother here, the review view
	 *      is read-only and very-wide lines just word-wrap (or
	 *      overflow into a per-section horizontal scroll, which
	 *      is fine for a quick read).
	 *   2. MergeView's chunk-align spacer pass still works as long
	 *      as each side reports its natural content height, which
	 *      `overflow: visible` lets through.
	 */
	.review-section :global(.cm-mergeView) {
		flex: 1;
		min-width: 0;
		overflow: visible;
	}
	.review-section :global(.cm-mergeViewEditors) {
		min-width: 0;
	}
	.review-section :global(.cm-editor) {
		outline: none;
	}
	.review-section :global(.cm-scroller) {
		overflow-x: auto;
	}
	/* Character-level change marker: see the matching rules in
	 * `DiffView.svelte` for the full rationale. Short version:
	 * the library default is a 2px bottom-edge gradient that
	 * reads as a loud underline doubled with LSP lint
	 * squigglies. Swap for a soft same-hue background
	 * (GitHub-style inline diff highlight) using our palette
	 * tokens so theme flips track. `!important` beats the
	 * package's themed rules without a fragile selector-
	 * specificity arms race. */
	.review-section :global(.cm-merge-b .cm-changedText) {
		background: color-mix(in srgb, var(--m-success) 22%, transparent) !important;
		border-radius: 2px;
	}
	.review-section :global(.cm-merge-a .cm-changedText),
	.review-section :global(.cm-deletedChunk .cm-deletedText) {
		background: color-mix(in srgb, var(--m-danger) 22%, transparent) !important;
		border-radius: 2px;
	}
	.review-section :global(.cm-merge-b .cm-deletedText) {
		background: color-mix(in srgb, var(--m-danger) 22%, transparent) !important;
	}
	/* Pure-added / pure-deleted lines: the line-level change is
	 * already conveyed by the gutter bar plus the line tint, so
	 * layering the per-character marker on top doubles up the
	 * same hue across the entire line. `diffPureChange.ts`
	 * adds the `.cm-moon-pure-change` line decoration on any
	 * line whose content is wholly inside `Change` spans —
	 * whole-chunk pure adds / deletes plus all-new / all-removed
	 * lines living inside an otherwise-modified chunk. Lines
	 * with surviving common substrings keep their per-character
	 * markers so the substring-vs-surrounding-text distinction
	 * stays visible. */
	.review-section :global(.cm-moon-pure-change .cm-changedText) {
		background: transparent !important;
	}
</style>
