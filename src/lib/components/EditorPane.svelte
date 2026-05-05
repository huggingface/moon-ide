<script lang="ts">
	import EditorTabs from './EditorTabs.svelte';
	import Editor from './Editor.svelte';
	import DiffView from './DiffView.svelte';
	import ImageView from './ImageView.svelte';
	import MarkdownView from './MarkdownView.svelte';
	import Welcome from './Welcome.svelte';
	import { open } from '@tauri-apps/plugin-dialog';
	import { workspace, type SplitSide } from '../state.svelte';
	import { isMarkdownPath } from '../util/markdown';

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
	// pane when it's either:
	//   - in diff mode (the user toggled it via tab button / palette
	//     / Ctrl-Shift-D / gutter click — `workspace.diffModes`), or
	//   - a deleted-file tab (nothing to edit; the HEAD blob shown
	//     as an against-empty diff is the only sensible view).
	const showDiff = $derived.by(() => {
		if (activeFile === null || activeFile.kind !== 'text') {
			return false;
		}
		return activeFile.isDeleted || workspace.diffModeFor(activeFile.path);
	});
	const showMarkdownPreview = $derived(
		activeFile !== null &&
			activeFile.kind === 'text' &&
			isMarkdownPath(activeFile.path) &&
			workspace.previewModeFor(activeFile.path) === 'preview' &&
			!showDiff,
	);
	// Show the "Add to Coder" hint only when this pane is showing
	// the file the workspace's `activeSelection` points at, and
	// only over surfaces that actually expose a CodeMirror
	// selection. Image and Markdown-preview can't produce one;
	// diff view's right (working-tree) pane can — see
	// `DiffView.svelte`'s `publishDiffSelection` — so we let it
	// through. The hint anchors to the pane's top-right corner,
	// which lands over the right pane in diff mode (where the
	// editable side lives).
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
		return selection.path === activeFile.path;
	});

	async function pickFolder() {
		const selected = await open({ directory: true, multiple: false });
		if (typeof selected !== 'string') {
			return;
		}
		await workspace.openLocal(selected);
	}

	function focus() {
		workspace.focusSide(side);
	}
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
	<div class="body">
		{#if activeFile?.kind === 'image'}
			<!-- Image / Diff / Markdown views build CodeMirror /
			     image state in `onMount` and don't watch `file.path`
			     internally — `Editor` is the only view that handles
			     path swaps in-place. Key the others on the path so a
			     tab change behind the same view kind (e.g. clicking
			     another modified file while the current one is in
			     diff mode) tears down the old instance and rebuilds.
			     Without the key the right-side merge editor's
			     update-listener still carries the original path in
			     its closure and ends up writing the new file's text
			     into the old file's buffer. -->
			{#key activeFile.path}
				<ImageView file={activeFile} />
			{/key}
		{:else if activeFile && showDiff}
			{#key activeFile.path}
				<DiffView file={activeFile} {side} />
			{/key}
		{:else if activeFile && showMarkdownPreview}
			{#key activeFile.path}
				<MarkdownView file={activeFile} />
			{/key}
		{:else if activeFile}
			<Editor file={activeFile} {side} />
		{:else}
			<Welcome onPickFolder={pickFolder} />
		{/if}
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
