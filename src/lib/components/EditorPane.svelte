<script lang="ts">
	import EditorTabs from './EditorTabs.svelte';
	import Editor from './Editor.svelte';
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
	const showMarkdownPreview = $derived(
		activeFile !== null &&
			activeFile.kind === 'text' &&
			isMarkdownPath(activeFile.path) &&
			workspace.previewModeFor(activeFile.path) === 'preview',
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

<div class="pane" role="group" tabindex="-1" onpointerdown={focus} onfocusin={focus}>
	<EditorTabs {side} />
	<div class="body">
		{#if activeFile?.kind === 'image'}
			<ImageView file={activeFile} />
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
