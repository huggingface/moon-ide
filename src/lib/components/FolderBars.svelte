<script lang="ts">
	import { projectCompose, projectComposeStateLabel } from '../projectCompose.svelte';
	import { workspace } from '../state.svelte';
	import ProjectComposePopover from './ProjectComposePopover.svelte';

	// Stacked folder bars — one row per folder bound into the workspace
	// — plus an `+ Add folder` row at the bottom. Click a bar to make
	// it active, hover for the `×` (remove) button, click the `+` to
	// pick another folder.
	//
	// `.indicator` slot: when the folder has its own
	// `docker-compose.yml`, a small dot reflects the per-folder
	// compose project's status (running / failed / paused / etc.).
	// Click opens a per-folder popover with start/stop/rebuild
	// actions and a service list. Folders without a compose file
	// get an empty (transparent) indicator so the row layout stays
	// stable across folders.
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
		{@const snap = projectCompose.snapshotFor(folder.path)}
		{@const hasCompose = snap?.compose_file != null}
		{@const composeState = snap?.status.state ?? null}
		{@const composeBusy = projectCompose.inFlightFor(folder.path) !== undefined}
		{@const popoverOpen = projectCompose.isPanelOpen(folder.path)}
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
			</button>
			{#if hasCompose}
				<button
					type="button"
					class="indicator state-{composeState ?? 'absent'}"
					class:busy={composeBusy}
					class:open={popoverOpen}
					title="Services: {projectComposeStateLabel(snap)}"
					aria-label="Services for {folder.name}: {projectComposeStateLabel(snap)}"
					aria-haspopup="dialog"
					aria-expanded={popoverOpen}
					onclick={(event) => {
						event.stopPropagation();
						projectCompose.togglePanel(folder.path);
					}}
				>
					<span class="dot" aria-hidden="true"></span>
				</button>
			{:else}
				<!-- Layout placeholder: keeps row width identical to
				     folders that do have a compose file so the `×`
				     button doesn't shift on hover. -->
				<span class="indicator placeholder" aria-hidden="true"></span>
			{/if}
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
			{#if popoverOpen && hasCompose}
				<ProjectComposePopover
					folderPath={folder.path}
					folderName={folder.name}
					onClose={() => projectCompose.closePanel(folder.path)}
				/>
			{/if}
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
		display: inline-flex;
		align-items: center;
		justify-content: center;
		width: 18px;
		height: 100%;
		background: transparent;
		border: none;
		padding: 0;
		cursor: pointer;
		color: var(--m-fg-muted);
	}
	.indicator.placeholder {
		cursor: default;
	}
	.indicator:not(.placeholder):hover,
	.indicator.open {
		background: var(--m-bg-1);
	}
	.indicator .dot {
		width: 8px;
		height: 8px;
		border-radius: 50%;
		background: currentColor;
		display: inline-block;
		transition: background 80ms;
	}
	/* Per-state colouring for the dot. Mirrors the state-text
	   colours used by the workspace status pip + popover so the
	   visual vocabulary stays consistent between the bottom bar
	   and the folder bars. `absent` (compose file present but
	   never brought up) is muted — neutral, "ready to start". */
	.indicator.state-absent .dot {
		background: var(--m-fg-subtle);
	}
	.indicator.state-creating .dot {
		background: var(--m-warning, var(--m-fg-muted));
		animation: pulse 1.6s ease-in-out infinite;
	}
	.indicator.state-running .dot {
		background: var(--m-success);
	}
	.indicator.state-paused .dot {
		background: var(--m-warning, var(--m-fg-muted));
	}
	.indicator.state-stopped .dot {
		background: var(--m-fg-muted);
	}
	.indicator.state-failed .dot {
		background: var(--m-danger);
	}
	/* `busy` = a per-folder lifecycle command (`up`, `pause`,
	   `down`, …) is in flight. Override both the colour and the
	   animation so the dot reads as "transitioning" rather than
	   layering a pulse on top of whatever the previous state was
	   — without this, a retry after a `failed` startup would
	   show a blinking _red_ dot, which conflates "actively
	   working on it" with "broken". */
	.indicator.busy .dot {
		background: var(--m-warning, var(--m-fg-muted));
		animation: pulse 1.2s ease-in-out infinite;
	}
	@keyframes pulse {
		0%,
		100% {
			opacity: 1;
		}
		50% {
			opacity: 0.4;
		}
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
