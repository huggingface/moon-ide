<script lang="ts">
	import { workspace } from '../state.svelte';

	// Stacked folder bars — one row per folder bound into the workspace
	// — plus an `+ Add folder` row at the bottom. Phase 2.5 surface:
	// click a bar to make it active, hover for the `×` (remove) button,
	// click the `+` to pick another folder. Compose status indicators
	// for each folder land with the Phase 2 container redesign and
	// will fill the empty `.indicator` slot — placed here now so the
	// row layout doesn't shift when they arrive.
	//
	// The active row's chevron points down (`▾`) and the file tree
	// renders directly underneath the bar in `Sidebar.svelte`. Inactive
	// rows show `▸` and are header-only.

	type Props = {
		onPickFolder: () => void | Promise<void>;
	};
	let { onPickFolder }: Props = $props();

	const folders = $derived(workspace.workspace?.folders ?? []);
	const activePath = $derived(workspace.activeFolderPath);
</script>

<ul class="bars" role="list">
	{#each folders as folder (folder.path)}
		{@const isActive = folder.path === activePath}
		<li class="bar" class:active={isActive}>
			<button
				type="button"
				class="bar-button"
				title={folder.path}
				aria-current={isActive ? 'true' : undefined}
				aria-expanded={isActive}
				onclick={() => void workspace.setActiveFolder(folder.path)}
			>
				<span class="chev" aria-hidden="true">{isActive ? '▾' : '▸'}</span>
				<span class="name">{folder.name}</span>
				<!-- Reserved slot for the Phase 2 container redesign:
				     a per-folder compose status indicator + quick
				     start/stop buttons. Empty in 2.5; the spacer keeps
				     the layout stable when 2.x lands. -->
				<span class="indicator" aria-hidden="true"></span>
			</button>
			<button
				type="button"
				class="remove"
				title="Remove from workspace"
				aria-label="Remove {folder.name} from workspace"
				onclick={(event) => {
					event.stopPropagation();
					void workspace.removeFolder(folder.path);
				}}
			>
				×
			</button>
		</li>
	{/each}
	<li class="add">
		<button
			type="button"
			class="add-button"
			data-folder-add-button
			title="Add folder…"
			onclick={() => void onPickFolder()}
		>
			<span class="plus" aria-hidden="true">+</span>
			<span class="add-label">{folders.length === 0 ? 'Open folder' : 'Add folder'}</span>
		</button>
	</li>
</ul>

<style>
	.bars {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		flex-shrink: 0;
		border-bottom: 1px solid var(--m-border);
	}
	.bar {
		position: relative;
		display: flex;
		align-items: stretch;
		min-height: 28px;
		border-bottom: 1px solid var(--m-border);
	}
	.bar:last-of-type {
		border-bottom: none;
	}
	.bar.active {
		background: var(--m-bg-overlay);
	}
	.bar-button {
		flex: 1;
		min-width: 0;
		display: flex;
		align-items: center;
		gap: 6px;
		background: transparent;
		border: none;
		color: var(--m-fg);
		text-align: left;
		font: inherit;
		padding: 0 8px;
		cursor: pointer;
		overflow: hidden;
	}
	.bar-button:hover {
		background: var(--m-bg-2);
	}
	.bar.active .bar-button {
		font-weight: 600;
	}
	.chev {
		width: 10px;
		flex-shrink: 0;
		color: var(--m-fg-muted);
		font-size: 10px;
		text-align: center;
	}
	.name {
		flex: 1;
		min-width: 0;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.indicator {
		flex-shrink: 0;
		min-width: 0;
		min-height: 0;
	}
	.remove {
		flex-shrink: 0;
		width: 22px;
		display: flex;
		align-items: center;
		justify-content: center;
		background: transparent;
		border: none;
		color: var(--m-fg-subtle);
		font-size: 14px;
		font-weight: 600;
		cursor: pointer;
		opacity: 0;
		transition: opacity 80ms;
	}
	/* Hover-reveal the `×`. Keep it permanently visible while focused
	   so keyboard users can find it without hunting. */
	.bar:hover .remove,
	.remove:focus-visible {
		opacity: 1;
	}
	.remove:hover {
		color: var(--m-danger);
		background: var(--m-bg-1);
	}
	.add {
		display: flex;
	}
	.add-button {
		flex: 1;
		display: flex;
		align-items: center;
		gap: 6px;
		background: transparent;
		border: none;
		color: var(--m-fg-muted);
		font: inherit;
		text-align: left;
		padding: 6px 8px;
		cursor: pointer;
	}
	.add-button:hover {
		background: var(--m-bg-2);
		color: var(--m-fg);
	}
	.plus {
		width: 10px;
		flex-shrink: 0;
		text-align: center;
		font-size: 14px;
		font-weight: 600;
	}
	.add-label {
		flex: 1;
		min-width: 0;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
</style>
