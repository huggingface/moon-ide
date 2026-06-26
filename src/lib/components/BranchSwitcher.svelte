<script lang="ts">
	import { workspace } from '../state.svelte';
	import type { BranchListEntry, BranchSwitchTarget } from '../protocol';
	import BranchIcon from './icons/BranchIcon.svelte';
	import SparklesIcon from './icons/SparklesIcon.svelte';

	let inputEl: HTMLInputElement | undefined = $state();
	let query = $state('');
	let selected = $state(0);
	// Keep the default branch list compact: real-world repos
	// hit the 20-row backend cap, but seeing all 20 on every
	// open is noisy when "the last few I touched" is the
	// 95th-percentile use case. Click "Show all" to expand.
	// Expansion auto-applies whenever the user types a query
	// (filtering ought to surface stale matches too).
	const DEFAULT_BRANCH_LIMIT = 10;
	let branchesExpanded = $state(false);

	// `[section, entry]` pairs flatten the two backend slices into a
	// single navigation array so the same `selected` index addresses
	// either kind. Filtered live by the query — the filter spans
	// branch name, PR number, PR title, PR author, head ref, commit
	// subject; type-to-filter is the team's main navigation gesture.
	type Row =
		| { kind: 'local'; entry: Extract<BranchListEntry, { kind: 'local' }> }
		| { kind: 'pr'; entry: Extract<BranchListEntry, { kind: 'pr' }> };

	const rows: Row[] = $derived.by(() => {
		const q = query.trim().toLowerCase();
		const out: Row[] = [];
		for (const entry of workspace.branchSwitcher.list.local) {
			if (entry.kind !== 'local') {
				continue;
			}
			if (q === '' || matchLocal(entry, q)) {
				out.push({ kind: 'local', entry });
			}
		}
		for (const entry of workspace.branchSwitcher.list.prs) {
			if (entry.kind !== 'pr') {
				continue;
			}
			if (q === '' || matchPr(entry, q)) {
				out.push({ kind: 'pr', entry });
			}
		}
		return out;
	});

	function matchLocal(entry: Extract<BranchListEntry, { kind: 'local' }>, q: string): boolean {
		return entry.name.toLowerCase().includes(q) || entry.lastCommitSubject.toLowerCase().includes(q);
	}

	function matchPr(entry: Extract<BranchListEntry, { kind: 'pr' }>, q: string): boolean {
		return (
			entry.title.toLowerCase().includes(q) ||
			entry.author.toLowerCase().includes(q) ||
			entry.headRef.toLowerCase().includes(q) ||
			String(entry.number).includes(q)
		);
	}

	$effect(() => {
		if (workspace.branchSwitcher.open && inputEl) {
			inputEl.focus();
			inputEl.select();
		}
		if (!workspace.branchSwitcher.open) {
			branchesExpanded = false;
		}
	});

	$effect(() => {
		// Reset selection whenever the filter changes shape; without
		// this, narrowing the list past the current cursor would
		// leave `selected` pointing past the end of `rows`.
		query;
		selected = 0;
	});

	function activate(index: number) {
		const row = rows[index];
		if (!row) {
			return;
		}
		if (row.kind === 'local' && row.entry.isCurrent) {
			workspace.closeBranchSwitcher();
			return;
		}
		const target: BranchSwitchTarget =
			row.kind === 'local' ? { kind: 'local', name: row.entry.name } : { kind: 'pr', number: row.entry.number };
		void workspace.switchToBranch(target);
	}

	// Spin up an isolated coder agent on this branch (ADR 0028): its
	// own git worktree + session, so it doesn't disturb the active
	// folder's checkout or other agents. For a PR row the branch is
	// its head ref (DWIM-created locally from the remote if needed).
	function startIsolatedAgent(event: MouseEvent, branch: string) {
		event.stopPropagation();
		workspace.closeBranchSwitcher();
		void workspace.newCoderWorktreeSession(branch);
	}

	function nextSelection(from: number): number {
		// When collapsed, the index range
		// `[visibleLocalCount, firstPrIndex)` (or
		// `[visibleLocalCount, rows.length)` when there are no
		// PRs) holds the hidden local rows; arrow nav skips them
		// either to the first PR or stops at the last visible
		// local row. The user can click "Show all" to make the
		// hidden rows reachable. The pinned default branch is the
		// one exception — it renders below the window and stays
		// keyboard-reachable.
		const candidate = from + 1;
		if (collapsed && candidate >= visibleLocalCount && candidate < firstPrIndex) {
			if (pinnedDefaultIndex !== -1 && from < pinnedDefaultIndex) {
				return pinnedDefaultIndex;
			}
			return firstPrIndex;
		}
		if (collapsed && firstPrIndex === -1 && candidate >= visibleLocalCount && candidate < localRowCount) {
			if (pinnedDefaultIndex !== -1 && from < pinnedDefaultIndex) {
				return pinnedDefaultIndex;
			}
			return Math.max(0, visibleLocalCount - 1);
		}
		return Math.min(candidate, Math.max(0, rows.length - 1));
	}

	function prevSelection(from: number): number {
		const candidate = from - 1;
		if (collapsed && candidate >= visibleLocalCount && candidate < firstPrIndex) {
			if (pinnedDefaultIndex !== -1 && from > pinnedDefaultIndex) {
				return pinnedDefaultIndex;
			}
			return Math.max(0, visibleLocalCount - 1);
		}
		return Math.max(candidate, 0);
	}

	function onKey(event: KeyboardEvent) {
		if (event.key === 'Escape') {
			event.preventDefault();
			workspace.closeBranchSwitcher();
			return;
		}
		if (event.key === 'ArrowDown') {
			event.preventDefault();
			selected = nextSelection(selected);
			return;
		}
		if (event.key === 'ArrowUp') {
			event.preventDefault();
			selected = prevSelection(selected);
			return;
		}
		if (event.key === 'Enter') {
			event.preventDefault();
			activate(selected);
			return;
		}
	}

	function onBackdrop(event: MouseEvent) {
		if (event.target === event.currentTarget) {
			workspace.closeBranchSwitcher();
		}
	}

	// Indices of the first PR row, used to insert a section header
	// at the right spot. -1 means "no PR rows in the current
	// filtered view"; the header is suppressed in that case.
	const firstPrIndex: number = $derived.by(() => rows.findIndex((r) => r.kind === 'pr'));
	const localRowCount: number = $derived.by(() => (firstPrIndex === -1 ? rows.length : firstPrIndex));
	// Filtering implies the user wants to see matches even if
	// they're far down the list, so collapse-mode is bypassed
	// while a query is active.
	const collapsed: boolean = $derived.by(
		() => !branchesExpanded && query.trim() === '' && localRowCount > DEFAULT_BRANCH_LIMIT,
	);
	const visibleLocalCount: number = $derived.by(() => (collapsed ? DEFAULT_BRANCH_LIMIT : localRowCount));
	// Index of the default-branch row within the flattened `rows`,
	// or -1 when there's no default in the current filtered view.
	// The backend always emits the default branch (even past the
	// recency cap), but it can land in the collapsed tail; we pin
	// it visible so switching back to main is always one click.
	const defaultLocalIndex: number = $derived.by(() =>
		rows.findIndex((r) => r.kind === 'local' && r.entry.isDefault && !r.entry.isCurrent),
	);
	// When collapsed, the default branch is shown out-of-band below
	// the visible window iff it falls into the hidden tail.
	const pinnedDefaultIndex: number = $derived.by(() =>
		collapsed && defaultLocalIndex >= visibleLocalCount && defaultLocalIndex < localRowCount ? defaultLocalIndex : -1,
	);
	// The pinned default still counts as visible, so it must not be
	// double-counted in the "Show N more" tally.
	const hiddenLocalCount: number = $derived.by(() => {
		const hidden = Math.max(0, localRowCount - visibleLocalCount);
		return pinnedDefaultIndex === -1 ? hidden : Math.max(0, hidden - 1);
	});

	// PR section's empty-state message. The frontend treats
	// `not_github` as "suppress the section entirely" — no
	// header, no message.
	const prEmptyMessage: string | null = $derived.by(() => {
		const status = workspace.branchSwitcher.list.prStatus;
		if (status.kind === 'ok') {
			return workspace.branchSwitcher.list.prs.length === 0 ? 'No open PRs.' : null;
		}
		if (status.kind === 'gh_missing') {
			return 'Install gh to see PR list. https://cli.github.com/';
		}
		if (status.kind === 'gh_not_authed') {
			return 'gh is signed out. Run `gh auth login` in a terminal.';
		}
		if (status.kind === 'failed') {
			return `gh pr list failed: ${status.detail}`;
		}
		return null;
	});

	const showPrSection: boolean = $derived.by(() => workspace.branchSwitcher.list.prStatus.kind !== 'not_github');
</script>

{#snippet localRow(entry: Extract<BranchListEntry, { kind: 'local' }>, idx: number)}
	<!-- Click activates a row; keyboard navigation lives on the
		 always-focused <input> above (Enter activates the
		 highlighted row, Arrow keys move it). Adding a per-row
		 keyboard handler would require focusing the <li> on hover,
		 which fights the input focus. Same pattern as
		 CommandPalette. -->
	<!-- svelte-ignore a11y_click_events_have_key_events -->
	<li
		class="result"
		class:selected={idx === selected}
		class:current={entry.isCurrent}
		role="option"
		aria-selected={idx === selected}
		onmousemove={() => (selected = idx)}
		onclick={() => activate(idx)}
	>
		<span class="title">
			<span class="kind-icon" title="Local branch">
				<BranchIcon size={13} />
			</span>
			<span class="branch-name">{entry.name}</span>
			{#if entry.isCurrent}<span class="badge">current</span>{:else if entry.isDefault}<span class="badge">default</span
				>{/if}
			{#if entry.lastCommitSubject !== ''}
				<span class="subject">{entry.lastCommitSubject}</span>
			{/if}
		</span>
		<span class="meta">{entry.committerDateRelative}</span>
		<button
			type="button"
			class="agent-action"
			title="Start an isolated coder agent on this branch (its own worktree)"
			aria-label="Start an isolated coder agent on {entry.name}"
			onclick={(e) => startIsolatedAgent(e, entry.name)}
		>
			<SparklesIcon size={13} />
		</button>
	</li>
{/snippet}

{#if workspace.branchSwitcher.open}
	<!-- Backdrop is a click target only; key events live on the
		 inner <input> for the same reason as CommandPalette. -->
	<div class="backdrop" role="presentation" onclick={onBackdrop} tabindex="-1">
		<div class="palette" role="dialog" aria-label="Switch branch">
			<div class="row">
				<span class="prefix">⎇</span>
				<input
					bind:this={inputEl}
					type="text"
					placeholder="Switch to branch or PR…"
					value={query}
					oninput={(e) => (query = e.currentTarget.value)}
					onkeydown={onKey}
				/>
				{#if workspace.branchSwitcher.loading || workspace.branchSwitcher.switching}
					<span class="loading">…</span>
				{/if}
			</div>
			<ul class="results" role="listbox">
				{#if localRowCount > 0}
					<li class="section">Branches</li>
				{/if}
				{#each rows.slice(0, visibleLocalCount) as row, i (`local-${i}`)}
					{#if row.kind === 'local'}
						{@render localRow(row.entry, i)}
					{/if}
				{/each}
				{#if pinnedDefaultIndex !== -1 && rows[pinnedDefaultIndex]?.kind === 'local'}
					<!-- Default branch pinned visible: it fell into the
							 collapsed tail (older than the recency window) but
							 switching back to it is the most common gesture, so
							 we always surface its row here. -->
					{@render localRow((rows[pinnedDefaultIndex] as Extract<Row, { kind: 'local' }>).entry, pinnedDefaultIndex)}
				{/if}
				{#if hiddenLocalCount > 0}
					<!-- "Show all" expander. Click-only on purpose: the
						 keyboard story is "type to filter" — typing any
						 query expands collapsed mode automatically. -->
					<!-- svelte-ignore a11y_click_events_have_key_events -->
					<li class="expand" role="option" aria-selected="false" onclick={() => (branchesExpanded = true)}>
						Show {hiddenLocalCount} more {hiddenLocalCount === 1 ? 'branch' : 'branches'}
					</li>
				{/if}
				{#if showPrSection}
					<li class="section">Open PRs</li>
					{#if firstPrIndex !== -1}
						{#each rows.slice(firstPrIndex) as row, i (`pr-${i}`)}
							{#if row.kind === 'pr'}
								{@const idx = firstPrIndex + i}
								<!-- Same a11y trade-off as the local rows above. -->
								<!-- svelte-ignore a11y_click_events_have_key_events -->
								<li
									class="result"
									class:selected={idx === selected}
									role="option"
									aria-selected={idx === selected}
									onmousemove={() => (selected = idx)}
									onclick={() => activate(idx)}
								>
									<span class="title">
										{#if row.entry.author !== ''}
											<!-- `https://github.com/<login>.png` is GitHub's
												 stable avatar redirect; no extra field on the
												 wire. The login is the most universal handle
												 we have (gh's `--json author` also exposes a
												 display name, but the team works in `@login`
												 most of the time). Decorative `alt` because the
												 tooltip carries the same info. -->
											<img
												class="avatar"
												src="https://github.com/{row.entry.author}.png?size=32"
												alt=""
												title={`@${row.entry.author}`}
												loading="lazy"
												referrerpolicy="no-referrer"
											/>
										{/if}
										<span class="pr-num">#{row.entry.number}</span>
										{#if row.entry.isDraft}<span class="badge">draft</span>{/if}
										<span class="pr-title">{row.entry.title}</span>
									</span>
									<span class="meta">
										<span class="date">{row.entry.updatedAtRelative}</span>
									</span>
									<button
										type="button"
										class="agent-action"
										title="Start an isolated coder agent on this PR's branch (its own worktree)"
										aria-label="Start an isolated coder agent on PR #{row.entry.number}"
										onclick={(e) => startIsolatedAgent(e, row.entry.headRef)}
									>
										<SparklesIcon size={13} />
									</button>
								</li>
							{/if}
						{/each}
					{:else if prEmptyMessage !== null}
						<li class="empty">{prEmptyMessage}</li>
					{/if}
				{/if}
				{#if rows.length === 0 && !workspace.branchSwitcher.loading && query.trim() !== ''}
					<li class="empty">No matches.</li>
				{/if}
			</ul>
			<div class="hint">
				<span>↵ switch · ↑↓ navigate · Esc close</span>
				<span class="modes" title="PR list filter — saved per folder">
					<button type="button" class:active={workspace.prScope === 'all'} onclick={() => workspace.setPrScope('all')}>
						All PRs
					</button>
					<button
						type="button"
						class:active={workspace.prScope === 'participating'}
						onclick={() => workspace.setPrScope('participating')}
					>
						Participating
					</button>
				</span>
			</div>
		</div>
	</div>
{/if}

<style>
	.backdrop {
		position: fixed;
		inset: 0;
		background: rgba(0, 0, 0, 0.45);
		z-index: 100;
		display: flex;
		align-items: flex-start;
		justify-content: center;
		padding-top: 80px;
	}
	.palette {
		width: min(640px, 90vw);
		max-height: 60vh;
		background: var(--m-bg-2);
		border: 1px solid var(--m-border-strong);
		border-radius: 8px;
		box-shadow: 0 24px 60px rgba(0, 0, 0, 0.6);
		display: flex;
		flex-direction: column;
		overflow: hidden;
	}
	.row {
		display: flex;
		align-items: center;
		gap: 8px;
		padding: 10px 12px;
		border-bottom: 1px solid var(--m-border);
	}
	.prefix {
		color: var(--m-fg-subtle);
		font-family: var(--m-font-mono);
		font-size: 14px;
		width: 14px;
		text-align: center;
	}
	input {
		flex: 1;
		background: transparent;
		border: none;
		color: var(--m-fg);
		font: inherit;
		outline: none;
	}
	input::placeholder {
		color: var(--m-fg-subtle);
	}
	.loading {
		color: var(--m-fg-subtle);
	}
	.results {
		list-style: none;
		margin: 0;
		padding: 4px 0;
		overflow-y: auto;
		flex: 1;
		min-height: 0;
	}
	.section {
		padding: 6px 14px 2px 14px;
		font-size: 10px;
		text-transform: uppercase;
		letter-spacing: 0.08em;
		color: var(--m-fg-subtle);
	}
	.result {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 12px;
		padding: 6px 14px;
		cursor: pointer;
		color: var(--m-fg-muted);
	}
	.result.selected {
		background: var(--m-accent);
		color: #0d1017;
	}
	.result.current {
		opacity: 0.7;
	}
	.title {
		display: flex;
		align-items: center;
		gap: 8px;
		overflow: hidden;
		min-width: 0;
		flex: 1;
	}
	.branch-name,
	.pr-title {
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
		min-width: 0;
	}
	.pr-title {
		flex: 1;
	}
	.branch-name {
		font-family: var(--m-font-mono);
		font-size: 12px;
		color: inherit;
		flex: 1;
	}
	.pr-num {
		font-family: var(--m-font-mono);
		font-size: 12px;
		color: inherit;
		opacity: 0.85;
		/* Keep the PR number whole even when the row is tight; the
		   title takes the truncation. */
		flex-shrink: 0;
		white-space: nowrap;
	}
	.subject,
	.pr-title {
		color: inherit;
		opacity: 0.75;
		font-size: 12px;
	}
	.meta {
		display: flex;
		align-items: center;
		gap: 8px;
		font-size: 11px;
		color: inherit;
		opacity: 0.7;
		white-space: nowrap;
	}
	/* Per-row "start an isolated agent on this branch" action.
	   Hidden until the row is hovered/selected so it doesn't compete
	   with the branch name; the row click still switches branches. */
	.agent-action {
		flex-shrink: 0;
		display: inline-flex;
		align-items: center;
		justify-content: center;
		width: 24px;
		height: 24px;
		padding: 0;
		border: 1px solid transparent;
		background: transparent;
		color: var(--m-fg-subtle);
		cursor: pointer;
		border-radius: 5px;
		opacity: 0;
		transition:
			opacity 0.1s,
			color 0.1s,
			background 0.1s,
			border-color 0.1s;
	}
	.result:hover .agent-action,
	.result.selected .agent-action {
		opacity: 1;
	}
	/* Clear, accent-tinted hover so the action reads as clickable
	   (the bare 3% overlay was effectively invisible). */
	.agent-action:hover {
		color: var(--m-accent-strong);
		background: var(--m-bg-3);
		border-color: var(--m-accent);
	}
	/* On the selected row (accent background, dark text) invert the
	   hover so it stays legible. */
	.result.selected .agent-action {
		color: #0d1017;
	}
	.result.selected .agent-action:hover {
		background: rgba(13, 16, 23, 0.18);
		border-color: rgba(13, 16, 23, 0.4);
		color: #0d1017;
	}
	.avatar {
		width: 16px;
		height: 16px;
		border-radius: 50%;
		display: block;
		opacity: 0.95;
	}
	.kind-icon {
		display: inline-flex;
		align-items: center;
		flex-shrink: 0;
		opacity: 0.7;
	}
	.result.selected .kind-icon {
		opacity: 1;
	}
	.badge {
		font-size: 10px;
		padding: 0 5px;
		border-radius: 3px;
		background: var(--m-bg-3);
		color: var(--m-fg-muted);
		text-transform: uppercase;
		letter-spacing: 0.05em;
		flex-shrink: 0;
	}
	.result.selected .badge {
		background: rgba(13, 16, 23, 0.2);
		color: #0d1017;
	}
	.empty {
		padding: 8px 14px;
		color: var(--m-fg-subtle);
		font-size: 12px;
	}
	.expand {
		padding: 6px 14px;
		font-size: 11px;
		color: var(--m-fg-subtle);
		cursor: pointer;
		text-align: left;
	}
	.expand:hover {
		color: var(--m-fg);
	}
	.hint {
		display: flex;
		align-items: center;
		justify-content: space-between;
		padding: 6px 12px;
		border-top: 1px solid var(--m-border);
		font-size: 11px;
		color: var(--m-fg-subtle);
	}
	.modes {
		display: flex;
		gap: 6px;
	}
	.modes button {
		padding: 2px 6px;
		border: 1px solid var(--m-border);
		border-radius: 4px;
		color: var(--m-fg-muted);
		font-size: 11px;
		background: transparent;
		cursor: pointer;
	}
	.modes button.active {
		background: var(--m-bg-3);
		color: var(--m-fg);
	}
</style>
