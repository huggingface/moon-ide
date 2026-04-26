<script lang="ts">
	import EditorTabs from './EditorTabs.svelte';
	import Editor from './Editor.svelte';
	import ImageView from './ImageView.svelte';
	import Welcome from './Welcome.svelte';
	import { open } from '@tauri-apps/plugin-dialog';
	import { workspace, type SplitSide } from '../state.svelte';

	type Props = { side: SplitSide };
	let { side }: Props = $props();

	const activePath: string | null = $derived(side === 'left' ? workspace.leftActive : workspace.rightActive);
	const activeFile = $derived.by(() => {
		if (activePath === null) {
			return null;
		}
		return workspace.openFiles.find((f) => f.path === activePath) ?? null;
	});
	const focused = $derived(workspace.focusedSide === side);

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

<div class="pane" class:focused role="group" tabindex="-1" onpointerdown={focus} onfocusin={focus}>
	<EditorTabs {side} />
	<div class="body">
		{#if activeFile?.kind === 'image'}
			<ImageView file={activeFile} />
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
		position: relative;
	}
	.pane.focused::before {
		content: '';
		position: absolute;
		inset: 0;
		border-top: 2px solid var(--m-accent);
		pointer-events: none;
	}
	.body {
		flex: 1;
		min-height: 0;
		display: flex;
	}
</style>
