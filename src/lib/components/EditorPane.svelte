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
			<ImageView file={activeFile} />
		{:else if activeFile && showDiff}
			<DiffView file={activeFile} {side} />
		{:else if activeFile && showMarkdownPreview}
			<MarkdownView file={activeFile} />
		{:else if activeFile}
			<Editor file={activeFile} {side} />
		{:else}
			<Welcome onPickFolder={pickFolder} />
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
	}
</style>
