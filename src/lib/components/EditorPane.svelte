<script lang="ts">
	import EditorTabs from './EditorTabs.svelte';
	import Editor from './Editor.svelte';
	import DiffView from './DiffView.svelte';
	import ImageView from './ImageView.svelte';
	import PdfView from './PdfView.svelte';
	import MarkdownView from './MarkdownView.svelte';
	import ReviewView from './ReviewView.svelte';
	import Welcome from './Welcome.svelte';
	import { open } from '@tauri-apps/plugin-dialog';
	import { workspace, type SplitSide } from '../state.svelte';
	import { isMarkdownPath } from '../util/markdown';
	import { isReviewPath } from '../util/reviewPath';
	import { frontendLog } from '../logs.svelte';

	type Props = { side: SplitSide };
	let { side }: Props = $props();

	// Single source of truth for "what is this pane showing". One
	// flat `$derived` reading the raw workspace state, instead of the
	// previous chain of `$derived` reading `$derived` reading
	// `$derived` (activePath → activeFile → showDiff/showMarkdown/…).
	//
	// That deep diamond was the bug behind "switch tabs, the strip
	// updates but the editor body keeps showing the old file": under
	// Svelte 5's lazy derived evaluation, a consuming effect (the
	// template render) could read a *stale* cached `activeFile` while
	// a sibling effect reading the raw `workspace.leftActive` saw the
	// fresh value. Folding everything the body needs into one object
	// computed from raw state means the template depends on exactly
	// one derived, recomputed atomically on every active-path /
	// openFiles / mode change — no intermediate edge to go stale.
	const view = $derived.by(() => {
		// `editorViewTick` is a plain top-level `$state` bumped by every
		// setter that changes what a pane renders. Reading it first
		// gives this derived a dependency that *cannot* go stale — the
		// per-folder field reads below go through the `activeFolderState`
		// getter funnel, whose leaf-field subscription was empirically
		// getting lost (body froze on a buffer no longer open while the
		// tab strip kept updating). The tick is the guaranteed
		// invalidation path; the field reads below still do the actual
		// work.
		void workspace.editorViewTick;
		// Read both signals unconditionally and up front. An earlier
		// version returned early when `path === null`, which meant the
		// derived never subscribed to `openFiles` on that run — so when
		// a freshly-opened folder later populated `openFiles` +
		// `leftActive`, nothing re-ran this derived and the body stayed
		// frozen on the empty state until a folder swap rebuilt the
		// graph. Touching `openFiles` every run keeps the subscription
		// alive regardless of which branch we take.
		const openFiles = workspace.openFiles;
		const path = side === 'left' ? workspace.leftActive : workspace.rightActive;
		const file = path === null ? null : (openFiles.find((f) => f.path === path) ?? null);
		if (file === null) {
			return { path, file: null, kind: 'welcome' as const };
		}
		if (file.kind === 'image') {
			return { path, file, kind: 'image' as const };
		}
		if (file.kind === 'pdf') {
			return { path, file, kind: 'pdf' as const };
		}
		// Review view: synthetic `review://…` buffer. Wins over
		// everything else for that path.
		if (isReviewPath(file.path)) {
			return { path, file, kind: 'review' as const };
		}
		// Diff view: explicit user gesture (tab toggle, palette,
		// Ctrl+Shift+D, gutter click, SCM changes-tree click on a
		// modified row). Deleted files are NOT force-routed here —
		// `Editor.svelte` shows their HEAD content read-only.
		if (file.kind === 'text' && workspace.diffModeFor(file.path)) {
			return { path, file, kind: 'diff' as const };
		}
		// Markdown preview: per-buffer toggle, suppressed when diff
		// or review already claimed the buffer (handled above).
		if (file.kind === 'text' && isMarkdownPath(file.path) && workspace.previewModeFor(file.path) === 'preview') {
			return { path, file, kind: 'markdown' as const };
		}
		return { path, file, kind: 'editor' as const };
	});

	// Show the "Add to Coder" hint only when this pane is showing the
	// file the workspace's `activeSelection` points at, and only over
	// surfaces that actually expose a CodeMirror selection. Image and
	// Markdown-preview can't produce one; diff view's right
	// (working-tree) pane can — see `DiffView.svelte`'s
	// `publishDiffSelection` — so we let it through. The review view
	// also publishes selections from its working-tree side (see
	// `ReviewSection.svelte`), so the hint is allowed there too — the
	// `activeFile.path` is the synthetic `review://…` token in that
	// mode, so we relax the path-equality check for the review
	// surface and just trust that the section produced the selection.
	// The hint anchors to the pane's top-right corner, which lands
	// over the right pane in diff mode and the top-right of the
	// stacked sections in review mode.
	const showCoderHint = $derived.by(() => {
		const selection = workspace.activeSelection;
		if (selection === null) {
			return false;
		}
		const file = view.file;
		if (file === null || file.kind !== 'text') {
			return false;
		}
		if (view.kind === 'markdown') {
			return false;
		}
		if (view.kind === 'review') {
			return true;
		}
		return selection.path === file.path;
	});

	async function pickFolder() {
		const selected = await open({ directory: true, multiple: false });
		if (typeof selected !== 'string') {
			return;
		}
		await workspace.openLocal(selected);
	}

	// Post-flush marker for folder-swap profiling: fires after this
	// pane has reconciled. The keyed Image/Diff/Markdown/Review
	// blocks tear down and rebuild on path change; the regular
	// Editor swaps state in place. Either way the mark lets us align
	// EditorPane reconciliation against the rest of the cascade in a
	// devtools timeline.
	$effect(() => {
		void view;
		performance.mark(`moon:editorPane.${side}.update`);
	});

	function focus() {
		workspace.focusSide(side);
	}

	// Holds the boundary's `reset` while the body is in its failed
	// state. The auto-reset effect below calls it on the next view
	// change so switching tabs recovers the pane without a click.
	let pendingBoundaryReset: (() => void) | null = $state(null);

	function onBodyError(error: unknown, reset: () => void) {
		const detail =
			error instanceof Error ? `${error.name}: ${error.message}\n${error.stack ?? '(no stack)'}` : String(error);
		frontendLog('runtime', 'error', `EditorPane(${side}) body boundary caught: path=${view.path ?? '∅'}\n${detail}`);
		pendingBoundaryReset = reset;
	}

	// Auto-recover on navigation: when the body has crashed and the
	// active path changes, tear down the failed subtree and rebuild
	// it for the new file rather than leaving the fallback up until
	// the manual "Reload view" button is clicked.
	$effect(() => {
		void view.path;
		const reset = pendingBoundaryReset;
		if (reset === null) {
			return;
		}
		pendingBoundaryReset = null;
		reset();
	});
</script>

<div
	class="pane"
	role="group"
	tabindex="-1"
	data-region={side === 'left' ? 'editor-left' : 'editor-right'}
	onpointerdown={focus}
	onfocusin={focus}
>
	<EditorTabs {side} />
	<!-- `data-view-*` are written by this template's render effect, so
	     they record what the pane's body *actually last committed*.
	     The "Debug: Dump Editor State" palette command compares them
	     against the raw workspace state to tell a frozen template
	     effect (state fresh, dataset stale) apart from broken state
	     (both stale). Costs nothing; keep them. -->
	<div class="body" data-view-path={view.path ?? ''} data-view-kind={view.kind}>
		<!-- Boundary so a throw inside a view component's render or
		     its child effects (Editor / DiffView / MarkdownView /
		     ReviewView / ImageView) is caught and surfaced instead
		     of silently detaching EditorPane's own reactive scope.
		     `onerror` logs the full error + stack to the `runtime`
		     diag source; the auto-reset effect rebuilds the body on
		     the next navigation. -->
		<svelte:boundary onerror={(error, reset) => onBodyError(error, reset)}>
			{#if view.file && view.kind === 'pdf'}
				{#key view.file.path}
					<PdfView file={view.file} />
				{/key}
			{:else if view.file && view.kind === 'image'}
				<!-- PDF / Image / Diff / Markdown / Review views build
			     CodeMirror / canvas / image state in `onMount` and don't
			     watch `file.path` internally — `Editor` is the
			     only view that handles path swaps in-place. Key
			     the others on the path so a tab change behind the
			     same view kind (e.g. clicking another modified
			     file while the current one is in diff mode) tears
			     down the old instance and rebuilds. Without the
			     key the right-side merge editor's update-listener
			     still carries the original path in its closure and
			     ends up writing the new file's text into the old
			     file's buffer. -->
				{#key view.file.path}
					<ImageView file={view.file} />
				{/key}
			{:else if view.file && view.kind === 'review'}
				{#key view.file.path}
					<ReviewView {side} />
				{/key}
			{:else if view.file && view.kind === 'diff'}
				{#key view.file.path}
					<DiffView file={view.file} {side} />
				{/key}
			{:else if view.file && view.kind === 'markdown'}
				{#key view.file.path}
					<MarkdownView file={view.file} {side} />
				{/key}
			{:else if view.file}
				<Editor file={view.file} {side} />
			{:else}
				<Welcome onPickFolder={pickFolder} />
			{/if}
			{#snippet failed(error, reset)}
				<!-- Visible fallback so the pane isn't a blank void
				     after a child crash. Clicking another tab (or any
				     state change that re-keys the body) calls `reset`
				     via the effect below; the button is the manual
				     escape hatch. -->
				<div class="body-error" role="alert">
					<p>The editor view hit an error.</p>
					<pre>{error instanceof Error ? error.message : String(error)}</pre>
					<button type="button" onclick={reset}>Reload view</button>
				</div>
			{/snippet}
		</svelte:boundary>
		{#if showCoderHint}
			<!-- Floating reminder for the Ctrl+L "add selection to
				 coder" gesture. Visible only when the workspace's
				 active selection belongs to *this* pane's file —
				 otherwise the user might have selected text in the
				 other split and we'd be advertising the gesture in
				 the wrong corner. Pointer-events disabled because
				 a click on the pill does nothing useful (the
				 gesture is keyboard-only); the hint shouldn't trap
				 a click that was meant to land in the editor. -->
			<div class="coder-hint" aria-hidden="true">
				<kbd>Ctrl+L</kbd>
				<span>Add selection to Coder</span>
			</div>
		{/if}
	</div>
</div>

<style>
	.pane {
		display: flex;
		flex-direction: column;
		flex: 1;
		min-width: 0;
		min-height: 0;
		background: var(--m-bg);
	}
	/* Focus indicator lives on the active tab's underline (bright on
	the focused side, muted on the unfocused side via `.active-blurred`).
	We used to also paint a 2px accent border-top on the focused pane,
	but in single-pane mode that was a redundant second copy of the same
	signal, and in split mode the bright-vs-muted tab underline already
	tells the panes apart at a glance. Removed for visual quiet. */
	.body {
		flex: 1;
		min-height: 0;
		display: flex;
		position: relative;
	}
	.body-error {
		flex: 1;
		display: flex;
		flex-direction: column;
		align-items: flex-start;
		gap: 12px;
		padding: 24px;
		overflow: auto;
		color: var(--m-fg);
	}
	.body-error pre {
		margin: 0;
		max-width: 100%;
		white-space: pre-wrap;
		word-break: break-word;
		font-family: var(--m-font-mono, monospace);
		font-size: 12px;
		color: var(--m-danger);
	}
	.body-error button {
		font-family: var(--m-font-ui);
		font-size: 12px;
		padding: 4px 12px;
		border: 1px solid var(--m-border);
		border-radius: 4px;
		background: var(--m-bg-1);
		color: var(--m-fg);
		cursor: pointer;
	}
	.body-error button:hover {
		background: var(--m-bg-overlay);
	}
	/* Floating "Ctrl+L Add selection to Coder" hint. Anchored to
	   the editor body's top-right corner, away from the file
	   tabs (which sit above `.body`) and clear of the editor's
	   own gutter / scrollbar. Pointer-events disabled — the
	   gesture is keyboard-only, and we don't want a stray click
	   on the pill to land here instead of the editor. */
	.coder-hint {
		position: absolute;
		top: 6px;
		right: 14px;
		z-index: 4;
		display: flex;
		align-items: center;
		gap: 6px;
		padding: 3px 6px 3px 4px;
		background: color-mix(in srgb, var(--m-bg-1) 92%, transparent);
		border: 1px solid var(--m-border);
		border-radius: 4px;
		font-size: 11px;
		color: var(--m-fg-muted);
		pointer-events: none;
		box-shadow: 0 1px 4px rgba(0, 0, 0, 0.16);
	}
	.coder-hint kbd {
		font: inherit;
		font-family: var(--m-font-mono, monospace);
		font-size: 10px;
		padding: 1px 4px;
		background: var(--m-bg-overlay);
		border: 1px solid var(--m-border);
		border-radius: 3px;
		color: var(--m-fg);
	}
</style>
