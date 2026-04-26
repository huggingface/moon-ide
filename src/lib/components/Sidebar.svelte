<script lang="ts">
	import FileTree from './FileTree.svelte';
	import { workspace } from '../state.svelte';

	type Props = {
		onPickFolder: () => void | Promise<void>;
	};
	let { onPickFolder }: Props = $props();
</script>

<div class="sidebar">
	<header class="header">
		<button class="folder-name" title="Open another folder" onclick={() => void onPickFolder()}>
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
