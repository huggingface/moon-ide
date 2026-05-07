<script lang="ts">
	import { coder } from '../coder.svelte';
	import { projectCompose, projectComposeStateLabel } from '../projectCompose.svelte';
	import { workspace } from '../state.svelte';
	import ContainerIcon from './icons/ContainerIcon.svelte';
	import SparklesIcon from './icons/SparklesIcon.svelte';
	import ProjectComposePopover from './ProjectComposePopover.svelte';

	// Stacked folder bars — one row per folder bound into the workspace
	// — plus an `+ Add folder` row at the bottom. Click a bar to make
	// it active, hover for the `×` (remove) button, click the `+` to
	// pick another folder.
	//
	// Each row exposes three passive readouts about the folder:
	//
	// - **Agent-state glyph** — an AI sparkle rendered right
	//   after the folder name. Two states:
	//     - **Running**: pulsing accent-coloured sparkle while a
	//       turn is in flight for the folder (drives attention
	//       through motion).
	//     - **Finished, not seen**: static amber sparkle after
	//       a turn ends in a *non-active* folder, persisting
	//       until the user clicks that folder bar to switch
	//       active. Lets a user juggling background agents see
	//       "this one's done, look at it" without missing the
	//       completion. Cleared in `coder.setActiveFolder`.
	//   Sits in the same per-row column as the git badges so it
	//   doesn't push the name around. Reads through
	//   `coder.busyForFolder` and `coder.attentionPendingForFolder`
	//   so the glyph tracks the bucket's reactive `$state`.
	// - **Git change badges** (`+N ~N -N`) — added / modified /
	//   deleted counts pulled from the per-folder
	//   `gitChangeSummaries` map. Refreshed on workspace hydrate,
	//   on every active-folder `refreshGitStatus` pass, and on
	//   coder `tool_result` events so an agent in folder A
	//   modifying folder B is visible from B's bar without B
	//   becoming active.
	// - **Container indicator** — when the folder ships a
	//   `docker-compose.yml`, a small container glyph reflects the
	//   project's compose state (running / failed / paused / …).
	//   Click opens a per-folder popover with start/stop/rebuild.
	//   Folders without compose get a placeholder so the row
	//   layout doesn't shift across folders.
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

	function summaryTitle(added: number, modified: number, deleted: number): string {
		const parts: string[] = [];
		if (added > 0) {
			parts.push(`${added} added/untracked`);
		}
		if (modified > 0) {
			parts.push(`${modified} modified`);
		}
		if (deleted > 0) {
			parts.push(`${deleted} deleted`);
		}
		return parts.join(', ');
	}
</script>

<ul class="bars" role="list">
	{#each folders as folder (folder.path)}
		{@const isActive = folder.path === activePath}
		{@const snap = projectCompose.snapshotFor(folder.path)}
		{@const hasCompose = snap?.compose_file != null}
		{@const composeState = snap?.status.state ?? null}
		{@const composeBusy = projectCompose.inFlightFor(folder.path) !== undefined}
		{@const popoverOpen = projectCompose.isPanelOpen(folder.path)}
		{@const summary = workspace.gitChangeSummaryFor(folder.path)}
		{@const added = summary?.added ?? 0}
		{@const modified = summary?.modified ?? 0}
		{@const deleted = summary?.deleted ?? 0}
		{@const hasChanges = added + modified + deleted > 0}
		{@const agentRunning = coder.busyForFolder(folder.path)}
		{@const agentDone = !agentRunning && coder.attentionPendingForFolder(folder.path)}
		{@const barTitle = agentRunning
			? `${folder.path}\n(agent running)`
			: agentDone
				? `${folder.path}\n(agent finished — click to view)`
				: folder.path}
		<li class="bar" class:active={isActive}>
			<button
				type="button"
				class="bar-button"
				title={barTitle}
				aria-current={isActive ? 'true' : undefined}
				aria-expanded={isActive}
				onclick={() => void workspace.setActiveFolder(folder.path)}
			>
				<span class="chev" aria-hidden="true">{isActive ? '▾' : '▸'}</span>
				<span class="name">{folder.name}</span>
				{#if agentRunning}
					<span class="agent-glyph running" aria-label="Agent running" title="Agent running">
						<SparklesIcon size={12} />
					</span>
				{:else if agentDone}
					<span
						class="agent-glyph done"
						aria-label="Agent finished — switch to this folder to view"
						title="Agent finished — click to view"
					>
						<SparklesIcon size={12} />
					</span>
				{/if}
			</button>
			{#if hasChanges}
				<span
					class="git-summary"
					title="Working tree: {summaryTitle(added, modified, deleted)}"
					aria-label="Git changes: {summaryTitle(added, modified, deleted)}"
				>
					{#if added > 0}
						<span class="badge added">+{added}</span>
					{/if}
					{#if modified > 0}
						<span class="badge modified">~{modified}</span>
					{/if}
					{#if deleted > 0}
						<span class="badge deleted">-{deleted}</span>
					{/if}
				</span>
			{/if}
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
					<ContainerIcon size={14} />
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
	/* Agent-state glyph — an AI sparkle right after the folder
	   name. Same SparklesIcon the SCM panel uses for AI
	   suggestions so the "magic-is-happening" vocabulary stays
	   consistent across the IDE. Two variants:

	   - `.running` — a turn is currently in flight for this
	     folder. Accent colour + opacity pulse reads as "live"
	     and earns the attention.
	   - `.done` — a turn finished in this folder while the user
	     was looking elsewhere, and the user hasn't visited the
	     folder since. Amber colour, *no animation*: the work is
	     done, so a pulse would over-claim attention; a static
	     hue is enough to say "this one's waiting on you" at a
	     glance. Clears as soon as the folder becomes active. */
	.agent-glyph {
		flex-shrink: 0;
		display: inline-flex;
		align-items: center;
		justify-content: center;
	}
	.agent-glyph.running {
		color: var(--m-accent);
		animation: agent-glyph-pulse 1.4s ease-in-out infinite;
	}
	.agent-glyph.done {
		color: var(--m-warning, var(--m-fg-muted));
	}
	@keyframes agent-glyph-pulse {
		0%,
		100% {
			opacity: 1;
		}
		50% {
			opacity: 0.45;
		}
	}
	.name {
		flex: 0 1 auto;
		min-width: 0;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	/* Inside the bar-button, after the name + optional agent glyph,
	   a flexible spacer eats remaining space so the next sibling
	   (currently nothing — git-summary lives outside the button)
	   doesn't get pulled in. Keeping this as a separate rule on
	   the button itself rather than a stray <div> so the markup
	   stays scannable. */
	.bar-button::after {
		content: '';
		flex: 1;
	}
	.git-summary {
		flex-shrink: 0;
		display: inline-flex;
		align-items: center;
		gap: 4px;
		padding: 0 6px;
		font-size: 11px;
		font-variant-numeric: tabular-nums;
		line-height: 1;
		pointer-events: auto;
	}
	.badge {
		display: inline-block;
	}
	/* Mirror Pierre's tree-tinting palette so a folder bar
	   showing `+3 ~1 -2` reads in the same colour vocabulary as
	   the file rows inside it. */
	.badge.added {
		color: var(--m-success);
	}
	.badge.modified {
		color: var(--m-warning, var(--m-fg-muted));
	}
	.badge.deleted {
		color: var(--m-danger);
	}
	.indicator {
		flex-shrink: 0;
		display: inline-flex;
		align-items: center;
		justify-content: center;
		width: 22px;
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
	/* Per-state colouring for the container icon. Mirrors the
	   state-text colours used by the workspace status pip +
	   popover so the visual vocabulary stays consistent between
	   the bottom bar and the folder bars. `absent` (compose file
	   present but never brought up) is muted — neutral, "ready
	   to start". */
	.indicator.state-absent {
		color: var(--m-fg-subtle);
	}
	.indicator.state-creating {
		color: var(--m-warning, var(--m-fg-muted));
		animation: pulse 1.6s ease-in-out infinite;
	}
	.indicator.state-running {
		color: var(--m-success);
	}
	.indicator.state-paused {
		color: var(--m-warning, var(--m-fg-muted));
	}
	.indicator.state-stopped {
		color: var(--m-fg-muted);
	}
	.indicator.state-failed {
		color: var(--m-danger);
	}
	/* `busy` = a per-folder lifecycle command (`up`, `pause`,
	   `down`, …) is in flight. Override both the colour and the
	   animation so the icon reads as "transitioning" rather than
	   layering a pulse on top of whatever the previous state was
	   — without this, a retry after a `failed` startup would
	   show a blinking _red_ glyph, which conflates "actively
	   working on it" with "broken". */
	.indicator.busy {
		color: var(--m-warning, var(--m-fg-muted));
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
