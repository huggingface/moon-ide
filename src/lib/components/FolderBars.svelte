<script lang="ts">
	import { coder } from '../coder.svelte';
	import type { WorkspaceFolder } from '../protocol';
	import { projectCompose, projectComposeStateLabel } from '../projectCompose.svelte';
	import { workspace } from '../state.svelte';
	import BranchIcon from './icons/BranchIcon.svelte';
	import ContainerIcon from './icons/ContainerIcon.svelte';
	import QuestionBubbleIcon from './icons/QuestionBubbleIcon.svelte';
	import SparklesIcon from './icons/SparklesIcon.svelte';
	import ProjectComposePopover from './ProjectComposePopover.svelte';

	// Stacked folder bars — one row per folder bound into the workspace
	// — plus an `+ Add folder` row at the bottom. Click a bar to make
	// it active, hover for the `×` (remove) button, click the `+` to
	// pick another folder.
	//
	// Each row exposes three passive readouts about the folder:
	//
	// - **Agent-state glyph** — an AI sparkle (or question bubble)
	//   rendered right after the folder name. Three states, in
	//   precedence order:
	//     - **Needs input**: a question-bubble glyph while a turn
	//       in the folder is parked on an `ask_user` prompt,
	//       waiting for the human to answer. Takes precedence over
	//       the running pulse — the turn is technically still in
	//       flight, but it isn't *working*, it's blocked on the
	//       user, and "answer me" is the more actionable signal.
	//       A gentle pulse on the accent hue earns the attention
	//       without the urgency of an error.
	//     - **Running**: pulsing accent-coloured sparkle while a
	//       turn is in flight (and not parked) for the folder
	//       (drives attention through motion).
	//     - **Finished, not seen**: static amber sparkle after
	//       a turn ends in a *non-active* folder, persisting
	//       until the user clicks that folder bar to switch
	//       active. Lets a user juggling background agents see
	//       "this one's done, look at it" without missing the
	//       completion. Cleared in `coder.setActiveFolder`.
	//   Sits in the same per-row column as the git badges so it
	//   doesn't push the name around. Reads through
	//   `coder.awaitingInputForFolder`, `coder.busyForFolder` and
	//   `coder.attentionPendingForFolder` so the glyph tracks the
	//   bucket's reactive `$state`.
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

	// Shortcut hint shown in the `+ Add folder` button's tooltip.
	// Same `⌘` vs `Ctrl` heuristic as the Welcome screen.
	const isMac =
		typeof navigator !== 'undefined' && /Mac|iPhone|iPad|iPod/.test(navigator.platform || navigator.userAgent);
	const addFolderTitle = isMac ? 'Add folder (⌘+Shift+A)' : 'Add folder (Ctrl+Shift+A)';

	type Props = {
		onPickFolder: () => void | Promise<void>;
	};
	let { onPickFolder }: Props = $props();

	const folders = $derived(workspace.workspace?.folders ?? []);
	const activePath = $derived(workspace.activeFolderPath);

	// One rendered row. Worktree-backed coder sessions (ADR 0028)
	// bind their checkout as a folder; we render those nested
	// (`depth: 1`) directly under the parent they branched from, with
	// the branch as the row label instead of the directory basename.
	type FolderRow = { folder: WorkspaceFolder; branch: string | null; depth: number };

	const orderedFolders = $derived.by((): FolderRow[] => {
		const all = folders;
		const childrenByParent = new Map<string, WorkspaceFolder[]>();
		for (const f of all) {
			if (f.origin.kind === 'worktree') {
				const list = childrenByParent.get(f.origin.parentPath) ?? [];
				list.push(f);
				childrenByParent.set(f.origin.parentPath, list);
			}
		}
		const rows: FolderRow[] = [];
		for (const f of all) {
			if (f.origin.kind === 'worktree') {
				continue; // emitted under its parent below
			}
			rows.push({ folder: f, branch: null, depth: 0 });
			for (const child of childrenByParent.get(f.path) ?? []) {
				rows.push({
					folder: child,
					branch: child.origin.kind === 'worktree' ? child.origin.branch : null,
					depth: 1,
				});
			}
		}
		// Orphan worktrees (parent not bound — shouldn't happen, but a
		// stale snapshot could) surface at top level so they're never
		// lost off-screen.
		for (const f of all) {
			const origin = f.origin;
			if (origin.kind === 'worktree' && !all.some((p) => p.path === origin.parentPath)) {
				rows.push({ folder: f, branch: origin.branch, depth: 0 });
			}
		}
		return rows;
	});

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
	{#each orderedFolders as row (row.folder.path)}
		{@const folder = row.folder}
		{@const isWorktree = row.depth > 0}
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
		{@const agentAwaitingInput = coder.awaitingInputForFolder(folder.path)}
		{@const agentRunning = !agentAwaitingInput && coder.busyForFolder(folder.path)}
		{@const agentDone = !agentAwaitingInput && !agentRunning && coder.attentionPendingForFolder(folder.path)}
		{@const barTitle = agentAwaitingInput
			? `${folder.path}\n(agent needs your input)`
			: agentRunning
				? `${folder.path}\n(agent running)`
				: agentDone
					? `${folder.path}\n(agent finished — click to view)`
					: folder.path}
		<li class="bar" class:active={isActive} class:worktree={isWorktree}>
			<button
				type="button"
				class="bar-button"
				title={isWorktree ? `${folder.path}\n(isolated worktree on ${row.branch})` : barTitle}
				aria-current={isActive ? 'true' : undefined}
				aria-expanded={isActive}
				onclick={() => void workspace.setActiveFolder(folder.path)}
			>
				{#if isWorktree}
					<span class="chev branch-glyph" aria-hidden="true"><BranchIcon size={12} /></span>
					<span class="name">{row.branch}</span>
				{:else}
					<span class="chev" aria-hidden="true">{isActive ? '▾' : '▸'}</span>
					<span class="name">{folder.name}</span>
				{/if}
				{#if agentAwaitingInput}
					<span
						class="agent-glyph awaiting"
						aria-label="Agent needs your input"
						title="Agent needs your input — click to answer"
					>
						<QuestionBubbleIcon size={12} />
					</span>
				{:else if agentRunning}
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
			title={addFolderTitle}
			onclick={() => void onPickFolder()}
		>
			<span class="plus" aria-hidden="true">+</span>
			<span class="add-label">Add folder</span>
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
	/* Active folder: an accent-tinted fill + a solid accent spine on
	   the left edge. The old 3%-white wash was barely distinguishable
	   from the inactive grey — especially for the muted worktree rows. */
	.bar.active {
		background: color-mix(in srgb, var(--m-accent) 14%, transparent);
		box-shadow: inset 2px 0 0 var(--m-accent);
	}
	/* Worktree-backed session folders (ADR 0028) render nested under
	   their parent: indented, muted, with a branch glyph. */
	.bar.worktree .bar-button {
		padding-left: 22px;
		color: var(--m-fg-muted);
	}
	/* When the worktree row is the active one, un-mute it so the
	   accent fill reads as a real selection rather than dim grey. */
	.bar.worktree.active .bar-button {
		color: var(--m-fg);
	}
	.bar.worktree.active .branch-glyph {
		color: var(--m-accent);
	}
	.bar.worktree .name {
		font-family: var(--m-font-mono, monospace);
		font-size: 11px;
	}
	.branch-glyph {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		color: var(--m-fg-muted);
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
	/* Agent-state glyph — an AI sparkle (or question bubble) right
	   after the folder name. The SparklesIcon matches what the SCM
	   panel uses for AI suggestions so the "magic-is-happening"
	   vocabulary stays consistent across the IDE. Three variants:

	   - `.awaiting` — a turn is parked on an `ask_user` prompt,
	     waiting for the human. A question-bubble glyph on the
	     accent hue with a *gentler* pulse than `.running`: it
	     needs the user's eye ("answer me") but isn't an error, so
	     the motion is calmer than the working pulse.
	   - `.running` — a turn is currently in flight (and not
	     parked) for this folder. Accent colour + opacity pulse
	     reads as "live" and earns the attention.
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
	.agent-glyph.awaiting {
		color: var(--m-accent);
		animation: agent-glyph-pulse 2.2s ease-in-out infinite;
	}
	.agent-glyph.running {
		color: var(--m-accent);
		animation: agent-glyph-pulse 1.4s ease-in-out infinite;
	}
	.agent-glyph.done {
		color: var(--m-warning, var(--m-fg-muted));
	}
	@media (prefers-reduced-motion: reduce) {
		.agent-glyph.awaiting,
		.agent-glyph.running {
			animation: none;
		}
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
