<script lang="ts">
	import FileTree from './FileTree.svelte';
	import { workspace } from '../state.svelte';

	type Props = {
		onPickFolder: () => void | Promise<void>;
	};
	let { onPickFolder }: Props = $props();

	let folderButton: HTMLButtonElement | undefined = $state();

	// Pull focus into the sidebar when WorkspaceState bumps the tick
	// (F6 cycle, Ctrl+0). When a workspace is open, FileTree.svelte
	// owns the focus pull — it has access to Pierre's instance and
	// can pierce the shadow DOM where the tree rows actually live.
	// Sidebar only steps in when there's no tree to focus into, i.e.
	// the empty-workspace state, where the only sensible target is
	// the header "Open folder" button.
	$effect(() => {
		const tick = workspace.sidebarFocusTick;
		if (tick === 0 || workspace.workspace !== null) {
			return;
		}
		queueMicrotask(() => folderButton?.focus());
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
<div class="sidebar" data-region="sidebar" tabindex="-1" onkeydown={onKeyDown}>
	<header class="header">
		<button
			bind:this={folderButton}
			class="folder-name"
			title="Open another folder"
			onclick={() => void onPickFolder()}
		>
			{#if workspace.workspace}
				<span class="dot"></span>
				<span class="name">{workspace.workspace.name}</span>
			{:else}
				<span class="empty">Open folder</span>
			{/if}
		</button>
	</header>
	<div class="tree">
		{#if workspace.workspace}
			<FileTree />
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
	.header {
		height: 36px;
		display: flex;
		align-items: center;
		padding: 0 8px;
		border-bottom: 1px solid var(--m-border);
		flex-shrink: 0;
	}
	.folder-name {
		width: 100%;
		text-align: left;
		display: flex;
		align-items: center;
		gap: 8px;
		overflow: hidden;
	}
	.name {
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.dot {
		width: 6px;
		height: 6px;
		border-radius: 50%;
		background: var(--m-accent);
		flex-shrink: 0;
	}
	.empty {
		color: var(--m-fg-muted);
	}
	.tree {
		flex: 1;
		min-height: 0;
		overflow: hidden;
		padding: 8px 0 4px;
	}
</style>
