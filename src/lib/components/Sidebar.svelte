<script lang="ts">
	import FileTree from './FileTree.svelte';
	import FolderBars from './FolderBars.svelte';
	import { workspace } from '../state.svelte';

	type Props = {
		onPickFolder: () => void | Promise<void>;
	};
	let { onPickFolder }: Props = $props();

	let sidebar: HTMLDivElement | undefined = $state();

	// Pull focus into the sidebar when WorkspaceState bumps the tick
	// (F6 cycle, Ctrl+0). When a folder is active, FileTree.svelte
	// owns the focus pull — it has access to Pierre's instance and
	// can pierce the shadow DOM where the tree rows actually live.
	// Sidebar steps in only when there's no tree to focus into (no
	// folder is active), where the fallback target is FolderBars'
	// `+ Add folder` button — discoverable and keyboard-reachable
	// from a single F6 hop.
	$effect(() => {
		const tick = workspace.sidebarFocusTick;
		if (tick === 0 || workspace.activeFolder !== null) {
			return;
		}
		queueMicrotask(() => {
			const addBtn = sidebar?.querySelector<HTMLButtonElement>('[data-folder-add-button]');
			addBtn?.focus();
		});
	});

	// Esc inside the sidebar yanks focus back to the active editor —
	// VSCode's behavior. Skipped while the user is typing in Pierre's
	// search input (or any other input/textarea) so Esc still serves
	// its native "clear/close" role there.
	function onKeyDown(event: KeyboardEvent) {
		if (event.key !== 'Escape') {
			return;
		}
		const ae = document.activeElement;
		if (ae instanceof HTMLInputElement || ae instanceof HTMLTextAreaElement) {
			return;
		}
		event.preventDefault();
		workspace.requestEditorFocus();
	}
</script>

<!-- svelte-ignore a11y_no_static_element_interactions -->
<div class="sidebar" data-region="sidebar" tabindex="-1" bind:this={sidebar} onkeydown={onKeyDown}>
	<FolderBars {onPickFolder} />
	<div class="tree">
		{#if workspace.activeFolder}
			<!-- Re-mount the tree on folder switch. Per-folder tree
			     state (expansion, scroll position) is intentionally not
			     preserved in 2.5 — adding cross-switch tree memoisation
			     is a Phase 7 follow-up. The per-folder *tab* state is
			     preserved through `WorkspaceState.folderStates` and
			     swaps in lock-step. -->
			{#key workspace.activeFolderPath}
				<FileTree />
			{/key}
		{/if}
	</div>
</div>

<style>
	.sidebar {
		display: flex;
		flex-direction: column;
		height: 100%;
		min-width: 0;
		outline: none;
	}
	.tree {
		flex: 1;
		min-height: 0;
		overflow: hidden;
		padding: 8px 0 4px;
	}
</style>
