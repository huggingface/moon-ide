<script lang="ts">
	// The pull request behind the review tab's diff (ADR 0113): what
	// GitHub shows above "Files changed", plus moonbot's minor notes
	// and the review threads as a jump list. Sits at the top of the
	// review scroll, above the file sections.
	import { openUrl } from '@tauri-apps/plugin-opener';
	import { githubMarkdown, openMarkdownLinks } from '../githubMarkdown';
	import type { PrCheckState, PrThread, PullRequestDetail } from '../protocol';
	import Conversation from './Conversation.svelte';
	import Pill from './Pill.svelte';
	import ExternalLinkIcon from './icons/ExternalLinkIcon.svelte';
	import PullRequestIcon from './icons/PullRequestIcon.svelte';
	import RefreshIcon from './icons/RefreshIcon.svelte';

	type Props = {
		pr: PullRequestDetail;
		/** Paths that have a section in the review; others can't be jumped to. */
		paths: ReadonlySet<string>;
		refreshing: boolean;
		onJump: (path: string) => void;
		onRefresh: () => void;
	};
	let { pr, paths, refreshing, onJump, onRefresh }: Props = $props();

	const STATE_TONE = { open: 'success', draft: 'neutral', merged: 'merged', closed: 'danger' } as const;
	const REVIEW_LABEL = {
		approved: ['success', 'Approved'],
		changes_requested: ['danger', 'Changes requested'],
		review_required: ['neutral', 'Review required'],
	} as const;
	const CHECK_ORDER: PrCheckState[] = ['failure', 'pending', 'success', 'skipped'];
	const CHECK_LABEL: Record<PrCheckState, string> = {
		failure: 'failing',
		pending: 'running',
		success: 'passing',
		skipped: 'skipped',
	};

	const checkCounts = $derived(
		CHECK_ORDER.map((state) => ({ state, count: pr.checks.filter((check) => check.state === state).length })).filter(
			(entry) => entry.count > 0,
		),
	);
	const sortedChecks = $derived(
		pr.checks.toSorted((a, b) => CHECK_ORDER.indexOf(a.state) - CHECK_ORDER.indexOf(b.state)),
	);
	const openThreads = $derived(pr.threads.filter((thread) => !thread.resolved));
	const resolvedThreads = $derived(pr.threads.length - openThreads.length);
	// Resolved threads are history; they are listed when asked for.
	let showResolved = $state(false);
	const listedThreads = $derived(showResolved ? pr.threads : openThreads);
	// The notes' line numbers are for the commit moonbot reviewed.
	const notesStale = $derived(pr.minorNotesSha !== null && !pr.headSha.startsWith(pr.minorNotesSha));

	function firstLine(thread: PrThread): string {
		const body = thread.comments[0]?.body ?? '';
		const line = body.split('\n').find((text) => text.trim() !== '') ?? '';
		return line.replace(/^#+\s*/, '');
	}

	function location(path: string, line: number | null): string {
		return line === null ? path : `${path}:${line}`;
	}
</script>

<section class="pr">
	<header class="head">
		<span class="state {pr.state}"><PullRequestIcon size={16} /></span>
		<h2>{pr.title} <span class="number">#{pr.number}</span></h2>
		<span class="spacer"></span>
		<button class="icon-btn" class:spinning={refreshing} title="Re-read the pull request" onclick={onRefresh}>
			<RefreshIcon size={14} />
		</button>
		<button class="icon-btn" title="Open on GitHub" onclick={() => void openUrl(pr.url)}>
			<ExternalLinkIcon size={14} />
		</button>
	</header>
	<div class="meta">
		<Pill tone={STATE_TONE[pr.state]}>{pr.state}</Pill>
		{#if pr.review !== null}
			<Pill tone={REVIEW_LABEL[pr.review][0]}>{REVIEW_LABEL[pr.review][1]}</Pill>
		{/if}
		{#each checkCounts as entry (entry.state)}
			<Pill
				tone={entry.state === 'failure'
					? 'danger'
					: entry.state === 'pending'
						? 'warning'
						: entry.state === 'success'
							? 'success'
							: 'neutral'}
			>
				{entry.count}
				{CHECK_LABEL[entry.state]}
			</Pill>
		{/each}
		<span class="refs">
			{#if pr.author !== null}{pr.author} wants to merge{/if}
			<code>{pr.headRef}</code> into <code>{pr.baseRef}</code>
		</span>
	</div>

	{#if pr.body.trim() !== ''}
		<details class="block" open>
			<summary>Description</summary>
			<!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions (delegated link clicks; the links themselves are focusable) -->
			<div class="markdown-body body" use:githubMarkdown={pr.body} onclick={openMarkdownLinks}></div>
		</details>
	{/if}

	{#if pr.conversation.length > 0}
		<details class="block" open>
			<summary>Conversation <span class="count">{pr.conversation.length}</span></summary>
			<Conversation comments={pr.conversation} />
		</details>
	{/if}

	{#if pr.minorNotes.length > 0}
		<details class="block" open>
			<summary>
				Moonbot notes <span class="count">{pr.minorNotes.length}</span>
				{#if pr.minorNotesSha !== null}
					<span class="hint">
						written against <code>{pr.minorNotesSha}</code>{#if notesStale}; the branch has moved since, so lines may be
							off{/if}
					</span>
				{/if}
			</summary>
			<ul>
				{#each pr.minorNotes as note, index (index)}
					<li>
						<button
							class="loc"
							disabled={!paths.has(note.path)}
							title={paths.has(note.path)
								? 'Jump to this file in the review'
								: 'This file has no changes in the review'}
							onclick={() => onJump(note.path)}
						>
							{location(note.path, note.line)}
						</button>
						<!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions (delegated link clicks) -->
						<div class="markdown-body note" use:githubMarkdown={note.text} onclick={openMarkdownLinks}></div>
					</li>
				{/each}
			</ul>
		</details>
	{/if}

	{#if pr.threads.length > 0}
		<details class="block" open={openThreads.length > 0}>
			<summary>
				Review threads <span class="count">{openThreads.length} open</span>
				{#if resolvedThreads > 0}<span class="hint">{resolvedThreads} resolved</span>{/if}
			</summary>
			<ul>
				{#each listedThreads as thread, index (index)}
					<li>
						<button
							class="loc"
							disabled={!paths.has(thread.path)}
							title={paths.has(thread.path)
								? 'Jump to this file in the review'
								: 'This file has no changes in the review'}
							onclick={() => onJump(thread.path)}
						>
							{location(thread.path, thread.line)}
						</button>
						<span class="thread-line">
							<strong>{thread.comments[0]?.author ?? 'someone'}</strong>
							{firstLine(thread)}
							{#if thread.resolved}<span class="hint">resolved</span>{/if}
							{#if thread.outdated}<span class="hint">outdated</span>{/if}
						</span>
						<button
							class="icon-btn"
							title="Open this thread on GitHub"
							onclick={() => void openUrl(thread.comments[0]?.url ?? pr.url)}
						>
							<ExternalLinkIcon size={12} />
						</button>
					</li>
				{/each}
			</ul>
			{#if resolvedThreads > 0}
				<button class="toggle" onclick={() => (showResolved = !showResolved)}>
					{showResolved ? 'Hide' : 'Show'}
					{resolvedThreads} resolved
				</button>
			{/if}
		</details>
	{/if}

	{#if pr.checks.length > 0}
		<details class="block" open={pr.checks.some((check) => check.state === 'failure')}>
			<summary>Checks <span class="count">{pr.checks.length}</span></summary>
			<ul class="checks">
				{#each sortedChecks as check, index (index)}
					<li>
						<span class="check-dot {check.state}" title={CHECK_LABEL[check.state]}></span>
						{#if check.url !== null}
							<button class="check-name link" onclick={() => void openUrl(check.url ?? pr.url)}>{check.name}</button>
						{:else}
							<span class="check-name">{check.name}</span>
						{/if}
						<span class="hint">{CHECK_LABEL[check.state]}</span>
					</li>
				{/each}
			</ul>
		</details>
	{/if}
</section>

<style>
	.pr {
		margin: var(--m-space-3) var(--m-space-3) var(--m-space-2);
		padding: var(--m-space-3) var(--m-space-4);
		border: 1px solid var(--m-border);
		border-radius: var(--m-radius-md);
		background: var(--m-bg-1);
		font-size: var(--m-text-md);
	}
	.head {
		display: flex;
		align-items: center;
		gap: var(--m-space-2);
	}
	h2 {
		margin: 0;
		min-width: 0;
		font-size: var(--m-text-lg);
		font-weight: 600;
	}
	.number {
		color: var(--m-fg-subtle);
		font-weight: 400;
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
	.spacer {
		flex: 1;
	}
	.meta {
		display: flex;
		align-items: center;
		flex-wrap: wrap;
		gap: var(--m-space-2);
		margin-top: var(--m-space-2);
	}
	.refs,
	.hint {
		color: var(--m-fg-subtle);
		font-size: var(--m-text-sm);
		font-weight: 400;
		letter-spacing: 0;
		text-transform: none;
	}
	code {
		font-family: var(--m-font-mono);
		font-size: var(--m-text-sm);
		color: var(--m-fg-muted);
	}

	.block {
		margin-top: var(--m-space-3);
		padding-top: var(--m-space-2);
		border-top: 1px solid var(--m-border);
	}
	summary {
		display: flex;
		align-items: baseline;
		gap: var(--m-space-2);
		cursor: pointer;
		user-select: none;
		font-size: var(--m-text-xs);
		font-weight: 600;
		letter-spacing: 0.06em;
		text-transform: uppercase;
		color: var(--m-fg-muted);
	}
	.count {
		padding: 0 6px;
		border-radius: var(--m-radius-pill);
		background: var(--m-bg-2);
		color: var(--m-fg-subtle);
		font-weight: 500;
		letter-spacing: 0;
		text-transform: none;
	}
	.body {
		margin-top: var(--m-space-2);
		max-width: 860px;
	}
	/* A description that opens with a rule (bots put one under their
	   marker comment) would double the panel's own divider. */
	.body :global(> hr:first-child) {
		display: none;
	}
	ul {
		margin: var(--m-space-2) 0 0;
		padding: 0;
		list-style: none;
	}
	li {
		display: flex;
		align-items: baseline;
		gap: var(--m-space-2);
		padding: 3px 0;
	}
	.loc {
		flex-shrink: 0;
		max-width: 45%;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
		padding: 0 6px;
		border: 1px solid var(--m-border);
		border-radius: var(--m-radius-sm);
		background: var(--m-bg-2);
		color: var(--m-fg-muted);
		font-family: var(--m-font-mono);
		font-size: var(--m-text-xs);
		cursor: pointer;
	}
	.loc:hover:not(:disabled) {
		color: var(--m-fg);
		border-color: var(--m-accent);
	}
	.loc:disabled {
		cursor: default;
		opacity: 0.7;
	}
	.note,
	.thread-line {
		min-width: 0;
		flex: 1;
	}
	.note :global(p) {
		margin: 0;
	}
	.thread-line {
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
		color: var(--m-fg-muted);
	}
	.thread-line strong {
		color: var(--m-fg);
		font-weight: 600;
	}

	.checks li {
		align-items: center;
	}
	.check-dot {
		flex-shrink: 0;
		width: 8px;
		height: 8px;
		border-radius: 50%;
		background: var(--m-fg-subtle);
	}
	.check-dot.success {
		background: var(--m-success);
	}
	.check-dot.failure {
		background: var(--m-danger);
	}
	.check-dot.pending {
		background: var(--m-warning);
	}
	.check-name {
		min-width: 0;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
		font-size: var(--m-text-sm);
	}
	.check-name.link {
		padding: 0;
		border: none;
		background: none;
		color: var(--m-fg);
		font: inherit;
		font-size: var(--m-text-sm);
		text-align: left;
		cursor: pointer;
	}
	.check-name.link:hover {
		color: var(--m-accent);
		text-decoration: underline;
	}

	.toggle {
		margin-top: var(--m-space-1);
		padding: 2px var(--m-space-2);
		border: none;
		border-radius: var(--m-radius-sm);
		background: transparent;
		color: var(--m-accent);
		font: inherit;
		font-size: var(--m-text-sm);
		cursor: pointer;
	}
	.toggle:hover {
		background: var(--m-bg-2);
	}
	.icon-btn {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		flex-shrink: 0;
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
</style>
