<script lang="ts">
	// The GitHub inbox tab (ADR 0112): pull requests and notification
	// threads that want the user, in one list the chips above it
	// narrow by why. Rows are a keyboard list; Enter opens a pull
	// request in its own tab, with no checkout and, inside its
	// project, no folder switch.
	import { onMount } from 'svelte';
	import { openUrl } from '@tauri-apps/plugin-opener';
	import { inbox } from '../inbox.svelte';
	import {
		CHIP_LABELS,
		INBOX_CATEGORIES,
		TAG_LABELS,
		quietTags,
		reasonLabel,
		relativeTime,
		shortRepo,
		snoozeLabel,
		snoozeUntil,
		type InboxChip,
	} from '../inboxSections';
	import type { InboxItem, InboxSource } from '../protocol';
	import { workspace, type SplitSide } from '../state.svelte';
	import Pill from './Pill.svelte';
	import BellIcon from './icons/BellIcon.svelte';
	import CheckIcon from './icons/CheckIcon.svelte';
	import ClockIcon from './icons/ClockIcon.svelte';
	import CloseIcon from './icons/CloseIcon.svelte';
	import CommentIcon from './icons/CommentIcon.svelte';
	import ExternalLinkIcon from './icons/ExternalLinkIcon.svelte';
	import IssueIcon from './icons/IssueIcon.svelte';
	import PullRequestIcon from './icons/PullRequestIcon.svelte';
	import RefreshIcon from './icons/RefreshIcon.svelte';
	import ReviewIcon from './icons/ReviewIcon.svelte';

	type Props = { side: SplitSide };
	let { side }: Props = $props();

	const SOURCE_LABELS: Record<InboxSource, string> = {
		review_requested: 'Review requested',
		watched: 'Watched authors',
		mentioned: 'Mentioned',
		mine: 'Mine',
		notification: 'Notifications',
	};

	let list: HTMLDivElement | undefined = $state();
	let filterInput: HTMLInputElement | undefined = $state();
	let selectedKey = $state<string | null>(null);
	let openingId = $state<string | null>(null);
	let authorDraft = $state('');
	let now = $state(Date.now());

	const rows = $derived(inbox.list.items);
	const counts = $derived(inbox.list.counts);
	// The snoozed chip appears with the first snoozed item, and stays
	// while it is on so it can be switched off again.
	const chips = $derived<InboxChip[]>(
		counts.snoozed > 0 || inbox.activeChips.has('snoozed') ? [...INBOX_CATEGORIES, 'snoozed'] : [...INBOX_CATEGORIES],
	);
	const selectedIndex = $derived(rows.findIndex((row) => row.item.id === selectedKey));
	const status = $derived(inbox.snapshot?.status.kind ?? 'ok');
	const failures = $derived(inbox.snapshot?.failures ?? []);

	onMount(() => {
		const detach = inbox.attachView();
		const clock = window.setInterval(() => {
			now = Date.now();
		}, 30_000);
		if (workspace.focusedSide === side) {
			list?.focus();
		}
		return () => {
			detach();
			window.clearInterval(clock);
		};
	});

	$effect(() => {
		const key = selectedKey;
		if (key === null || !list) {
			return;
		}
		list.querySelector<HTMLElement>(`[data-key="${CSS.escape(key)}"]`)?.scrollIntoView({ block: 'nearest' });
	});

	/** Project folder whose repository `item` is a pull request of,
	 * when the workspace has it bound. Anything else has no tab here. */
	function projectFolder(item: InboxItem): string | null {
		if (item.kind !== 'pull_request' || item.number === null) {
			return null;
		}
		return inbox.folderFor(item.repo);
	}

	async function open(item: InboxItem) {
		if (openingId !== null) {
			return;
		}
		const project = projectFolder(item);
		if (project === null || item.number === null) {
			openOnGitHub(item);
			return;
		}
		openingId = item.id;
		try {
			// The tab reads the pull request from the active folder's
			// repository, so any checkout of the project will do; only
			// another project means moving.
			const here = workspace.activeProjectPath === project;
			if (!here) {
				await workspace.setActiveFolder(project);
			}
			if (workspace.activeProjectPath !== project) {
				return;
			}
			// This pane's side means nothing in another folder's layout.
			workspace.openPrTab(item.number, item.title, here ? side : undefined);
			void inbox.markRead(item);
		} finally {
			openingId = null;
		}
	}

	function openOnGitHub(item: InboxItem) {
		void openUrl(item.url);
	}

	/** Snooze `item`, or wake it when it is already snoozed. */
	function toggleSnooze(item: InboxItem, span: 'tomorrow' | 'next_week') {
		if (inbox.snoozedIds.has(item.id)) {
			inbox.unsnooze(item.id);
			return;
		}
		inbox.snooze(item, snoozeUntil(span, new Date()));
	}

	/** Run `action` on the selected row, then put the cursor on the
	 * row that takes its place (the acted-on row usually leaves). */
	function actAndKeepCursor(action: () => void) {
		const index = selectedIndex;
		action();
		queueMicrotask(() => {
			selectedKey = rows[Math.min(index, rows.length - 1)]?.item.id ?? null;
			// A clicked row holds the focus, and takes it along when it
			// leaves; the next key has to land on the list.
			list?.focus({ preventScroll: true });
		});
	}

	function move(delta: number) {
		if (rows.length === 0) {
			return;
		}
		const from = selectedIndex === -1 ? (delta > 0 ? -1 : rows.length) : selectedIndex;
		const next = Math.max(0, Math.min(rows.length - 1, from + delta));
		selectedKey = rows[next]?.item.id ?? null;
	}

	function onKeydown(event: KeyboardEvent) {
		// A focused row button handles its own Enter / Space.
		if (event.ctrlKey || event.metaKey || event.altKey || event.target instanceof HTMLButtonElement) {
			return;
		}
		const selected = selectedIndex === -1 ? null : (rows[selectedIndex]?.item ?? null);
		switch (event.key) {
			case 'j':
			case 'ArrowDown':
				move(1);
				break;
			case 'k':
			case 'ArrowUp':
				move(-1);
				break;
			case 'Enter':
				if (selected !== null) {
					void open(selected);
				}
				break;
			case 'o':
				if (selected !== null) {
					openOnGitHub(selected);
				}
				break;
			case 'e':
				if (selected !== null) {
					actAndKeepCursor(() => void inbox.markRead(selected));
				}
				break;
			case 's':
			case 'w':
				if (selected !== null) {
					actAndKeepCursor(() => toggleSnooze(selected, event.key === 's' ? 'tomorrow' : 'next_week'));
				}
				break;
			case 'r':
				void inbox.refresh();
				break;
			case '/':
				filterInput?.focus();
				filterInput?.select();
				break;
			case '0':
				inbox.clearChips();
				break;
			case '1':
			case '2':
			case '3':
			case '4':
			case '5': {
				const chip = INBOX_CATEGORIES[Number(event.key) - 1];
				if (chip !== undefined) {
					inbox.toggleChip(chip);
				}
				break;
			}
			default:
				return;
		}
		event.preventDefault();
	}

	function onFieldKeydown(event: KeyboardEvent) {
		if (event.key === 'Escape') {
			event.preventDefault();
			list?.focus();
		}
	}

	function addAuthor() {
		const author = authorDraft.trim();
		if (author === '') {
			return;
		}
		inbox.setWatchedAuthors([...inbox.watchedAuthors, author]);
		authorDraft = '';
	}

	function removeAuthor(author: string) {
		inbox.setWatchedAuthors(inbox.watchedAuthors.filter((watched) => watched !== author));
	}

	function avatar(url: string): string {
		return `${url}${url.includes('?') ? '&' : '?'}s=40`;
	}

	function stateTitle(item: InboxItem): string {
		if (item.kind === 'issue') {
			return 'Issue';
		}
		if (item.kind === 'other') {
			return 'Notification';
		}
		switch (item.state) {
			case 'draft':
				return 'Draft pull request';
			case 'merged':
				return 'Merged pull request';
			case 'closed':
				return 'Closed pull request';
			default:
				return 'Open pull request';
		}
	}
</script>

<div class="inbox">
	<header class="head">
		<h1>Inbox</h1>
		{#if inbox.snapshot?.viewer}
			<span class="viewer">@{inbox.snapshot.viewer}</span>
		{/if}
		<input
			class="field filter"
			bind:this={filterInput}
			bind:value={inbox.filter}
			placeholder="Filter by title, repo, author, branch"
			spellcheck="false"
			autocomplete="off"
			aria-label="Filter"
			onkeydown={onFieldKeydown}
		/>
		<div class="watching">
			<span class="label">Watching</span>
			{#each inbox.watchedAuthors as author (author)}
				<Pill tone="accent">
					{author}
					<button class="chip-x" title="Stop watching {author}" onclick={() => removeAuthor(author)}>
						<CloseIcon size={10} />
					</button>
				</Pill>
			{/each}
			<input
				class="field author"
				bind:value={authorDraft}
				placeholder="login or name[bot]"
				spellcheck="false"
				autocomplete="off"
				aria-label="Watch an author"
				onkeydown={(event) => {
					if (event.key === 'Enter') {
						event.preventDefault();
						addAuthor();
						return;
					}
					onFieldKeydown(event);
				}}
			/>
		</div>
		<span class="spacer"></span>
		{#if inbox.fetchedAt !== null}
			<span class="updated">updated {relativeTime(new Date(inbox.fetchedAt).toISOString(), now)}</span>
		{/if}
		<button class="icon-btn" class:spinning={inbox.loading} title="Refresh (r)" onclick={() => void inbox.refresh()}>
			<RefreshIcon size={14} />
		</button>
	</header>

	{#if inbox.error !== null}
		<p class="notice danger">Could not reach GitHub: {inbox.error}</p>
	{:else if status === 'gh_missing'}
		<p class="notice">The inbox reads GitHub through the <code>gh</code> CLI, which isn't installed on this machine.</p>
	{:else if status === 'gh_not_authed'}
		<p class="notice">
			<code>gh</code> isn't logged in. Run <code>gh auth login</code> in a terminal, then refresh.
		</p>
	{:else if failures.length > 0}
		<p class="notice">
			Could not refresh {failures.map((failure) => SOURCE_LABELS[failure.source]).join(', ')} ({failures[0]?.detail}).
			Those rows are from the previous refresh.
		</p>
	{/if}

	{#if inbox.snapshot !== null && (inbox.snapshot.items.length > 0 || inbox.activeChips.size > 0)}
		<!-- A click must not take focus from the list: the keys keep working. -->
		<div class="chips" role="group" aria-label="Show only">
			{#each chips as chip, index (chip)}
				<button
					class="chip"
					class:active={inbox.activeChips.has(chip)}
					aria-pressed={inbox.activeChips.has(chip)}
					title={index < INBOX_CATEGORIES.length ? `Toggle (${index + 1})` : undefined}
					onmousedown={(event) => event.preventDefault()}
					onclick={() => inbox.toggleChip(chip)}
				>
					<span class="dot dot-{chip}"></span>
					{CHIP_LABELS[chip]}
					<span class="count">{counts[chip]}</span>
				</button>
			{/each}
			{#if inbox.activeChips.size > 0}
				<button
					class="chip-clear"
					title="Show everything (0)"
					onmousedown={(event) => event.preventDefault()}
					onclick={() => inbox.clearChips()}
				>
					Clear
				</button>
			{/if}
		</div>
	{/if}

	<div class="scroll" bind:this={list} tabindex="0" role="listbox" aria-label="GitHub inbox" onkeydown={onKeydown}>
		{#each rows as row (row.item.id)}
			{@const item = row.item}
			{@const hasTab = projectFolder(item) !== null}
			<!-- svelte-ignore a11y_click_events_have_key_events (keys are handled on the listbox) -->
			<div
				class="row"
				class:selected={item.id === selectedKey}
				class:unread={item.notification !== null}
				data-key={item.id}
				role="option"
				tabindex="-1"
				aria-selected={item.id === selectedKey}
				onclick={() => (selectedKey = item.id)}
				ondblclick={() => void open(item)}
			>
				<span class="unread-dot" title={item.notification !== null ? 'Unread' : undefined}></span>
				<span class="state {item.kind === 'pull_request' ? (item.state ?? 'open') : 'plain'}" title={stateTitle(item)}>
					{#if item.kind === 'pull_request'}
						<PullRequestIcon size={14} />
					{:else if item.kind === 'issue'}
						<IssueIcon size={14} />
					{:else}
						<BellIcon size={14} />
					{/if}
				</span>
				<span class="title">{item.title}</span>
				{#if item.number !== null}
					<span class="number">#{item.number}</span>
				{/if}
				<span class="meta" title={item.repo}>
					{shortRepo(item.repo)}{#if item.author !== null}<span class="sep">·</span>{item.author}{/if}
				</span>
				<span class="tags">
					{#if row.snoozed}
						<Pill>until {snoozeLabel(inbox.snoozedUntil(item.id) ?? '', new Date(now))}</Pill>
					{/if}
					{#if item.notification !== null}
						<Pill tone="accent">{reasonLabel(item.notification.reason)}</Pill>
					{/if}
					{#each quietTags(row) as tag (tag)}
						<Pill title={CHIP_LABELS[tag]}>{TAG_LABELS[tag]}</Pill>
					{/each}
				</span>
				<span class="spacer"></span>
				{#if openingId === item.id}
					<span class="opening">Opening…</span>
				{:else}
					<span class="actions">
						{#if hasTab}
							<button class="icon-btn" title="Open the pull request (Enter)" onclick={() => void open(item)}>
								<ReviewIcon size={14} />
							</button>
						{/if}
						<button class="icon-btn" title="Open on GitHub (o)" onclick={() => openOnGitHub(item)}>
							<ExternalLinkIcon size={14} />
						</button>
						{#if item.notification !== null}
							<button class="icon-btn" title="Mark read (e)" onclick={() => void inbox.markRead(item)}>
								<CheckIcon size={14} />
							</button>
						{/if}
						<button
							class="icon-btn"
							title={row.snoozed ? 'Wake (s)' : 'Snooze until tomorrow (s); next week is w'}
							onclick={() => toggleSnooze(item, 'tomorrow')}
						>
							<ClockIcon size={14} />
						</button>
					</span>
				{/if}
				<!-- The trailing columns keep their width when empty so
					 they line up down the list. -->
				<span class="diffstat">
					{#if item.additions !== null && item.deletions !== null}
						<span class="add">+{item.additions}</span>
						<span class="del">−{item.deletions}</span>
					{/if}
				</span>
				<span class="comments" title={item.commentCount ? `${item.commentCount} comments` : undefined}>
					{#if item.commentCount}
						<CommentIcon size={12} />{item.commentCount}
					{/if}
				</span>
				<span class="review {item.review ?? 'none'}">
					{#if item.review === 'approved'}
						<span title="Approved"><CheckIcon size={13} /></span>
					{:else if item.review === 'changes_requested'}
						<span title="Changes requested"><CloseIcon size={13} /></span>
					{/if}
				</span>
				<span
					class="ci {item.ci ?? 'none'}"
					title={item.ci === null
						? undefined
						: `Checks: ${item.ci === 'success' ? 'passing' : item.ci === 'failure' ? 'failing' : 'running'}`}
				></span>
				{#if item.authorAvatarUrl !== null}
					<img class="avatar" src={avatar(item.authorAvatarUrl)} alt="" title={item.author ?? undefined} />
				{:else}
					<span class="avatar"></span>
				{/if}
				<span class="time">{relativeTime(item.notification?.updatedAt ?? item.updatedAt, now)}</span>
			</div>
		{:else}
			{#if inbox.snapshot === null}
				<p class="empty">{inbox.loading ? 'Loading…' : ''}</p>
			{:else if status === 'ok' && inbox.snapshot.repos.length === 0}
				<p class="empty">No folder in this workspace is a GitHub repository.</p>
			{:else if status === 'ok' && inbox.filter.trim() !== ''}
				<p class="empty">Nothing matches the filter.</p>
			{:else if status === 'ok' && inbox.activeChips.size > 0}
				<p class="empty">Nothing in the selected categories.</p>
			{:else if status === 'ok'}
				<p class="empty">Nothing is waiting on you.</p>
			{/if}
		{/each}
	</div>

	<footer class="hints">
		<span><kbd>j</kbd><kbd>k</kbd> move</span>
		<span><kbd>Enter</kbd> open</span>
		<span><kbd>o</kbd> GitHub</span>
		<span><kbd>e</kbd> mark read</span>
		<span><kbd>s</kbd><kbd>w</kbd> snooze a day / a week</span>
		<span><kbd>r</kbd> refresh</span>
		<span><kbd>/</kbd> filter</span>
		<span><kbd>1</kbd>–<kbd>5</kbd> toggle a chip, <kbd>0</kbd> clears</span>
	</footer>
</div>

<style>
	.inbox {
		flex: 1;
		min-width: 0;
		min-height: 0;
		display: flex;
		flex-direction: column;
		background: var(--m-bg);
		font-size: var(--m-text-md);
	}

	.head {
		display: flex;
		align-items: center;
		gap: var(--m-space-3);
		padding: var(--m-space-3) var(--m-space-4);
		border-bottom: 1px solid var(--m-border);
		flex-wrap: wrap;
	}
	h1 {
		margin: 0;
		font-size: var(--m-text-lg);
		font-weight: 600;
	}
	.viewer,
	.updated,
	.label {
		color: var(--m-fg-subtle);
		font-size: var(--m-text-sm);
	}
	.spacer {
		flex: 1;
	}
	.field {
		font: inherit;
		font-size: var(--m-text-sm);
		color: var(--m-fg);
		background: var(--m-bg-1);
		border: 1px solid var(--m-border);
		border-radius: var(--m-radius-md);
		padding: var(--m-space-1) var(--m-space-2);
		min-width: 0;
	}
	.field:focus {
		outline: none;
		border-color: var(--m-accent);
	}
	.filter {
		width: 260px;
	}
	.author {
		width: 150px;
	}
	.watching {
		display: flex;
		align-items: center;
		gap: var(--m-space-2);
		flex-wrap: wrap;
	}
	.chip-x {
		display: inline-flex;
		padding: 0;
		border: none;
		background: none;
		color: inherit;
		cursor: pointer;
		opacity: 0.7;
	}
	.chip-x:hover {
		opacity: 1;
	}

	.icon-btn {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		width: 24px;
		height: 24px;
		padding: 0;
		border: none;
		border-radius: var(--m-radius-md);
		background: transparent;
		color: var(--m-fg-muted);
		cursor: pointer;
	}
	.icon-btn:hover {
		background: var(--m-bg-3);
		color: var(--m-fg);
	}
	.spinning :global(svg) {
		animation: spin 0.9s linear infinite;
	}
	@keyframes spin {
		to {
			transform: rotate(360deg);
		}
	}

	.notice {
		margin: 0;
		padding: var(--m-space-2) var(--m-space-4);
		border-bottom: 1px solid var(--m-border);
		background: var(--m-bg-1);
		color: var(--m-fg-muted);
		font-size: var(--m-text-sm);
	}
	.notice.danger {
		color: var(--m-danger);
	}
	code {
		font-family: var(--m-font-mono);
	}

	.scroll {
		flex: 1;
		min-height: 0;
		overflow-y: auto;
		padding: var(--m-space-2) var(--m-space-2) var(--m-space-4);
		outline: none;
		container-type: inline-size;
	}

	.chips {
		display: flex;
		align-items: center;
		flex-wrap: wrap;
		gap: var(--m-space-2);
		padding: var(--m-space-2) var(--m-space-4);
		border-bottom: 1px solid var(--m-border);
	}
	.chip {
		display: inline-flex;
		align-items: center;
		gap: 6px;
		height: 24px;
		padding: 0 var(--m-space-1) 0 var(--m-space-2);
		border: 1px solid var(--m-border);
		border-radius: var(--m-radius-pill);
		background: transparent;
		color: var(--m-fg-muted);
		font: inherit;
		font-size: var(--m-text-sm);
		cursor: pointer;
	}
	.chip:hover {
		background: var(--m-bg-1);
		color: var(--m-fg);
	}
	.chip.active {
		border-color: color-mix(in srgb, var(--m-accent) 55%, transparent);
		background: var(--m-accent-tint);
		color: var(--m-fg);
	}
	.chip:focus-visible,
	.chip-clear:focus-visible {
		outline: 1px solid var(--m-accent);
		outline-offset: 1px;
	}
	.count {
		min-width: 18px;
		padding: 0 5px;
		border-radius: var(--m-radius-pill);
		background: var(--m-bg-2);
		color: var(--m-fg-subtle);
		font-size: var(--m-text-xs);
		font-variant-numeric: tabular-nums;
		line-height: 16px;
		text-align: center;
	}
	.chip.active .count {
		background: color-mix(in srgb, var(--m-accent) 22%, transparent);
		color: var(--m-fg);
	}
	.chip-clear {
		padding: 2px var(--m-space-2);
		border: none;
		border-radius: var(--m-radius-md);
		background: transparent;
		color: var(--m-fg-subtle);
		font: inherit;
		font-size: var(--m-text-sm);
		cursor: pointer;
	}
	.chip-clear:hover {
		color: var(--m-fg);
		background: var(--m-bg-1);
	}
	.dot {
		width: 6px;
		height: 6px;
		border-radius: 50%;
		background: var(--m-fg-subtle);
	}
	.dot-unread {
		background: var(--m-accent);
	}
	.dot-review {
		background: var(--m-warning);
	}
	.dot-watched {
		background: var(--m-merged);
	}
	.dot-mentioned {
		background: var(--m-fg-muted);
	}
	.dot-mine {
		background: var(--m-success);
	}

	.row {
		position: relative;
		display: flex;
		align-items: center;
		gap: var(--m-space-2);
		height: 32px;
		padding: 0 var(--m-space-2);
		border-radius: var(--m-radius-md);
		border: 1px solid transparent;
		cursor: default;
		outline: none;
	}
	.row:hover {
		background: var(--m-bg-1);
	}
	.row.selected {
		background: var(--m-bg-2);
		border-color: color-mix(in srgb, var(--m-accent) 55%, transparent);
	}
	.unread-dot {
		flex-shrink: 0;
		width: 6px;
		height: 6px;
		border-radius: 50%;
	}
	.unread .unread-dot {
		background: var(--m-accent);
	}
	.state {
		display: inline-flex;
		flex-shrink: 0;
		color: var(--m-fg-subtle);
	}
	.state.open {
		color: var(--m-success);
	}
	.state.merged {
		color: var(--m-merged);
	}
	.state.closed {
		color: var(--m-danger);
	}
	.title {
		min-width: 0;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
		color: var(--m-fg-muted);
	}
	.unread .title {
		color: var(--m-fg);
		font-weight: 500;
	}
	.number,
	.meta,
	.time,
	.comments,
	.opening {
		flex-shrink: 0;
		color: var(--m-fg-subtle);
		font-size: var(--m-text-sm);
		white-space: nowrap;
	}
	.meta {
		min-width: 0;
		max-width: 30%;
		overflow: hidden;
		text-overflow: ellipsis;
		flex-shrink: 1;
	}
	.sep {
		margin: 0 5px;
	}
	.tags {
		display: inline-flex;
		gap: var(--m-space-1);
		flex-shrink: 0;
	}
	/* Laid over the row's trailing columns rather than beside the
	   title, so selecting a row never re-truncates it. */
	.actions {
		display: none;
		position: absolute;
		top: 0;
		bottom: 0;
		right: var(--m-space-2);
		align-items: center;
		padding-left: var(--m-space-3);
		background: inherit;
	}
	.row:hover .actions,
	.row.selected .actions {
		display: inline-flex;
	}
	.diffstat {
		display: inline-flex;
		gap: 5px;
		flex-shrink: 0;
		width: 76px;
		justify-content: flex-end;
		font-family: var(--m-font-mono);
		font-size: var(--m-text-xs);
		font-variant-numeric: tabular-nums;
	}
	.add {
		color: var(--m-success);
	}
	.del {
		color: var(--m-danger);
	}
	.comments {
		display: inline-flex;
		align-items: center;
		gap: 3px;
		width: 30px;
	}
	.review {
		display: inline-flex;
		flex-shrink: 0;
		width: 14px;
		justify-content: center;
	}
	.review.approved {
		color: var(--m-success);
	}
	.review.changes_requested {
		color: var(--m-danger);
	}
	.ci {
		flex-shrink: 0;
		width: 8px;
		height: 8px;
		border-radius: 50%;
	}
	.ci.success {
		background: var(--m-success);
	}
	.ci.failure {
		background: var(--m-danger);
	}
	.ci.pending {
		background: var(--m-warning);
	}
	.avatar {
		flex-shrink: 0;
		width: 18px;
		height: 18px;
		border-radius: 50%;
		background: var(--m-bg-3);
	}
	.time {
		width: 30px;
		text-align: right;
		font-variant-numeric: tabular-nums;
	}
	/* Next to an open coder panel the pane is too narrow for every
	 * column: the size of the change goes first, so the title and the
	 * reason tags keep their room. */
	@container (max-width: 680px) {
		.diffstat,
		.comments {
			display: none;
		}
	}

	.empty {
		margin: 48px 0;
		text-align: center;
		color: var(--m-fg-subtle);
	}

	.hints {
		display: flex;
		flex-wrap: wrap;
		gap: 2px var(--m-space-4);
		padding: var(--m-space-1) var(--m-space-4);
		border-top: 1px solid var(--m-border);
		color: var(--m-fg-subtle);
		font-size: var(--m-text-xs);
	}
	.hints span {
		white-space: nowrap;
	}
	kbd {
		display: inline-block;
		margin-right: 3px;
		padding: 0 4px;
		border: 1px solid var(--m-border);
		border-radius: var(--m-radius-sm);
		background: var(--m-bg-1);
		font-family: var(--m-font-mono);
		font-size: 10px;
	}
</style>
