<script lang="ts">
	import EditorTabs from './EditorTabs.svelte';
	import Editor from './Editor.svelte';
	import DiffView from './DiffView.svelte';
	import ImageView from './ImageView.svelte';
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

	const activePath: string | null = $derived(side === 'left' ? workspace.leftActive : workspace.rightActive);
	const activeFile = $derived.by(() => {
		if (activePath === null) {
			return null;
		}
		return workspace.openFiles.find((f) => f.path === activePath) ?? null;
	});
	// Diff-view wins over markdown-preview. A buffer hits the diff
	// pane only when the user has explicitly asked for it — via the
	// tab toggle, the command palette, `Ctrl-Shift-D`, the gutter
	// click, or (for `modified` files) a click in the SCM
	// changes-only tree. Deleted-file tabs used to be force-routed
	// here on the rationale that "there's no working tree to edit",
	// but a single read-only Editor showing the HEAD content is
	// what users actually want for "what was in this file before it
	// was deleted?" — a side-by-side against an empty pane just
	// halves the reading space. `Editor.svelte` flips itself
	// read-only when `file.isDeleted`, so the normal view is safe.
	const showReview = $derived(activeFile !== null && isReviewPath(activeFile.path));
	const showDiff = $derived.by(() => {
		if (activeFile === null || activeFile.kind !== 'text') {
			return false;
		}
		if (showReview) {
			return false;
		}
		return workspace.diffModeFor(activeFile.path);
	});
	const showMarkdownPreview = $derived(
		activeFile !== null &&
			activeFile.kind === 'text' &&
			isMarkdownPath(activeFile.path) &&
			workspace.previewModeFor(activeFile.path) === 'preview' &&
			!showDiff &&
			!showReview,
	);
	// Show the "Add to Coder" hint only when this pane is showing
	// the file the workspace's `activeSelection` points at, and
	// only over surfaces that actually expose a CodeMirror
	// selection. Image and Markdown-preview can't produce one;
	// diff view's right (working-tree) pane can — see
	// `DiffView.svelte`'s `publishDiffSelection` — so we let it
	// through. The review view also publishes selections from its
	// working-tree side (see `ReviewSection.svelte`), so the hint
	// is allowed there too — the `activeFile.path` is the synthetic
	// `review://…` token in that mode, so we relax the
	// path-equality check for the review surface and just trust
	// that the section produced the selection. The hint anchors
	// to the pane's top-right corner, which lands over the right
	// pane in diff mode and the top-right of the stacked sections
	// in review mode.
	const showCoderHint = $derived.by(() => {
		const selection = workspace.activeSelection;
		if (selection === null) {
			return false;
		}
		if (activeFile === null || activeFile.kind !== 'text') {
			return false;
		}
		if (showMarkdownPreview) {
			return false;
		}
		if (showReview) {
			return true;
		}
		return selection.path === activeFile.path;
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
	// Editor swaps state in place. Either way the mark lets us
	// align EditorPane reconciliation against the rest of the
	// cascade in a devtools timeline.
	$effect(() => {
		void activePath;
		void showReview;
		void showDiff;
		void showMarkdownPreview;
		performance.mark(`moon:editorPane.${side}.update`);
		const branch =
			activeFile === null
				? 'welcome'
				: activeFile.kind === 'image'
					? 'image'
					: showReview
						? 'review'
						: showDiff
							? 'diff'
							: showMarkdownPreview
								? 'markdown'
								: 'editor';
		frontendLog(
			'editor.swap',
			'debug',
			`EditorPane(${side}) activePath=${activePath ?? '∅'} branch=${branch} ` +
				`activeFile.path=${activeFile?.path ?? '∅'} showDiff=${showDiff} ` +
				`showReview=${showReview} showMd=${showMarkdownPreview}`,
		);
	});

	function focus() {
		workspace.focusSide(side);
	}

	// Read directly in the markup (below) so this derived lands in
	// the *template* render effect, not a side `$effect`. If the
	// body freezes on a tab switch while the strip updates, the
	// template effect is the thing that stopped — and this log line
	// (which only the template effect can trigger) going silent
	// while `setActive` keeps firing pins the blame on the render
	// effect's reactive scope rather than on a stray side effect.
	const bodyTrace = $derived.by(() => {
		const branch =
			activeFile === null
				? 'welcome'
				: activeFile.kind === 'image'
					? 'image'
					: showReview
						? 'review'
						: showDiff
							? 'diff'
							: showMarkdownPreview
								? 'markdown'
								: 'editor';
		frontendLog(
			'editor.swap',
			'debug',
			`EditorPane(${side}) RENDER activePath=${activePath ?? '∅'} branch=${branch} file=${activeFile?.path ?? '∅'}`,
		);
		return '';
	});

	// Holds the boundary's `reset` while the body is in its failed
	// state. Cleared once we've reset. The auto-reset effect below
	// calls it on the next `activePath` change so switching tabs
	// recovers the pane without a manual click.
	let pendingBoundaryReset: (() => void) | null = $state(null);

	function onBodyError(error: unknown, reset: () => void) {
		const detail =
			error instanceof Error ? `${error.name}: ${error.message}\n${error.stack ?? '(no stack)'}` : String(error);
		frontendLog(
			'editor.swap',
			'error',
			`EditorPane(${side}) body boundary caught: activePath=${activePath ?? '∅'}\n${detail}`,
		);
		pendingBoundaryReset = reset;
	}

	// Auto-recover on navigation. When the body has crashed and the
	// user switches to another tab (or any state change moves
	// `activePath`), tear down the failed subtree and rebuild it for
	// the new file. Without this the boundary would keep showing the
	// fallback until the manual "Reload view" button is clicked,
	// which is the exact "editor body froze on tab switch" symptom
	// we're chasing.
	$effect(() => {
		void activePath;
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
	<div class="body" data-body-trace={bodyTrace}>
		<!-- Boundary so a throw inside a view component's render or
		     its child effects (Editor / DiffView / MarkdownView /
		     ReviewView / ImageView) is caught and surfaced instead
		     of silently detaching EditorPane's own reactive scope.
		     Symptom we're chasing: a tab switch updates the strip
		     but the editor body freezes — consistent with a child
		     effect crashing the flush. `onerror` logs the full
		     error + stack to the `editor.swap` diag source; `reset`
		     lets the next state change rebuild the body. -->
		<svelte:boundary onerror={(error, reset) => onBodyError(error, reset)}>
			{#if activeFile?.kind === 'image'}
				<!-- Image / Diff / Markdown / Review views build
			     CodeMirror / image state in `onMount` and don't
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
				{#key activeFile.path}
					<ImageView file={activeFile} />
				{/key}
			{:else if activeFile && showReview}
				{#key activeFile.path}
					<ReviewView {side} />
				{/key}
			{:else if activeFile && showDiff}
				{#key activeFile.path}
					<DiffView file={activeFile} {side} />
				{/key}
			{:else if activeFile && showMarkdownPreview}
				{#key activeFile.path}
					<MarkdownView file={activeFile} {side} />
				{/key}
			{:else if activeFile}
				<Editor file={activeFile} {side} />
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
