<script lang="ts" module>
	import type { IssueDetail } from '../protocol';

	// The last answer per issue: coming back to the tab paints this at
	// once while the re-read runs.
	const lastLoaded = new Map<string, IssueDetail>();
</script>

<script lang="ts">
	// An issue in its own tab (ADR 0117): description and comments,
	// read-only. Replying and triage happen on GitHub.
	import { onMount } from 'svelte';
	import { openUrl } from '@tauri-apps/plugin-opener';
	import { githubMarkdown, openMarkdownLinks, repoUrlOf } from '../githubMarkdown';
	import { relativeTime } from '../inboxSections';
	import { ipc } from '../ipc';
	import { formatError } from '../protocol';
	import { workspace, type SplitSide } from '../state.svelte';
	import { parseIssuePath } from '../util/issuePath';
	import Conversation from './Conversation.svelte';
	import Pill from './Pill.svelte';
	import ExternalLinkIcon from './icons/ExternalLinkIcon.svelte';
	import IssueIcon from './icons/IssueIcon.svelte';
	import RefreshIcon from './icons/RefreshIcon.svelte';

	type Props = { side: SplitSide };
	let { side }: Props = $props();

	let scroller: HTMLDivElement | undefined = $state();

	const tabPath = $derived(side === 'left' ? workspace.leftActive : workspace.rightActive);
	const ref = $derived(tabPath === null ? null : parseIssuePath(tabPath));
	const number = $derived(ref?.number ?? null);
	// What the tab strip already knows, to head the page before
	// GitHub answers.
	const tabName = $derived(workspace.openFiles.find((file) => file.path === tabPath)?.name ?? '');
	const cacheKey = $derived(tabPath ?? '');

	let issue = $state<IssueDetail | null>(null);
	let loadError = $state('');
	let loading = $state(false);
	let loadToken = 0;

	async function load() {
		const wanted = ref;
		if (wanted === null) {
			loadError = 'Not an issue.';
			return;
		}
		const key = cacheKey;
		const token = ++loadToken;
		loading = true;
		try {
			const result = await ipc.issue.open(wanted.folder, wanted.number);
			if (token !== loadToken) {
				return;
			}
			lastLoaded.set(key, result);
			issue = result;
			loadError = '';
		} catch (err) {
			if (token !== loadToken) {
				return;
			}
			// A failed re-read keeps what is on screen.
			if (issue === null) {
				loadError = formatError(err);
			} else {
				workspace.flash(`Could not re-read the issue: ${formatError(err)}`);
			}
		} finally {
			if (token === loadToken) {
				loading = false;
			}
		}
	}

	onMount(() => {
		scroller?.focus({ preventScroll: true });
		issue = lastLoaded.get(cacheKey) ?? null;
		void load();
		return () => {
			loadToken++;
		};
	});

	const now = Date.now();
</script>

<!-- svelte-ignore a11y_no_noninteractive_tabindex -->
<div class="issue-view" bind:this={scroller} tabindex="0" role="region" aria-label="Issue">
	{#if issue === null}
		<header class="head">
			<h2>{tabName || (number === null ? 'Issue' : `#${number}`)}</h2>
		</header>
		<div class="empty" class:error={loadError !== ''}>
			{#if loadError !== ''}
				Could not open the issue: {loadError}
				<button type="button" class="btn" onclick={() => void load()} disabled={loading}>Try again</button>
			{:else}
				Loading issue…
			{/if}
		</div>
	{:else}
		<header class="head">
			<span class="state" class:closed={!issue.open}><IssueIcon size={16} /></span>
			<h2>{issue.title} <span class="number">#{issue.number}</span></h2>
			<span class="spacer"></span>
			<button class="icon-btn" class:spinning={loading} title="Re-read the issue" onclick={() => void load()}>
				<RefreshIcon size={14} />
			</button>
			<button class="icon-btn" title="Open on GitHub" onclick={() => issue !== null && void openUrl(issue.url)}>
				<ExternalLinkIcon size={14} />
			</button>
		</header>
		<div class="meta">
			<Pill tone={issue.open ? 'success' : 'merged'}>{issue.open ? 'open' : 'closed'}</Pill>
			{#each issue.labels as label (label.name)}
				<Pill>
					<span class="swatch" style:background="#{label.color}"></span>
					{label.name}
				</Pill>
			{/each}
			<span class="byline">
				{issue.author ?? 'someone'} opened this
				<span title={issue.createdAt}>{relativeTime(issue.createdAt, now)}</span>
				{#if issue.assignees.length > 0}
					· assigned to {issue.assignees.join(', ')}
				{/if}
			</span>
		</div>

		{#if issue.body.trim() !== ''}
			<!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions (delegated link clicks; the links themselves are focusable) -->
			<div
				class="markdown-body body"
				use:githubMarkdown={{ text: issue.body, repoUrl: repoUrlOf(issue.url) }}
				onclick={openMarkdownLinks}
			></div>
		{:else}
			<p class="none">No description.</p>
		{/if}

		{#if issue.comments.length > 0}
			<h3>
				{issue.comments.length} comment{issue.comments.length === 1 ? '' : 's'}
			</h3>
			<Conversation comments={issue.comments} repoUrl={repoUrlOf(issue.url)} />
		{/if}
	{/if}
</div>

<style>
	.issue-view {
		flex: 1;
		min-width: 0;
		min-height: 0;
		overflow-y: auto;
		background: var(--m-bg);
		color: var(--m-fg);
		padding: var(--m-space-4) 24px 24px;
		outline: none;
	}
	.head {
		display: flex;
		align-items: center;
		gap: var(--m-space-2);
		max-width: 860px;
	}
	h2 {
		margin: 0;
		min-width: 0;
		font-size: var(--m-text-lg);
		font-weight: 600;
		overflow-wrap: anywhere;
	}
	.number {
		color: var(--m-fg-subtle);
		font-weight: 400;
	}
	.state {
		display: inline-flex;
		flex-shrink: 0;
		color: var(--m-success);
	}
	.state.closed {
		color: var(--m-merged);
	}
	.spacer {
		flex: 1;
	}
	.meta {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: var(--m-space-2);
		max-width: 860px;
		margin-top: var(--m-space-2);
		font-size: var(--m-text-sm);
		color: var(--m-fg-muted);
	}
	.swatch {
		width: 8px;
		height: 8px;
		border-radius: 50%;
	}
	.body {
		max-width: 860px;
		margin-top: var(--m-space-4);
		overflow-wrap: anywhere;
	}
	.none {
		margin: var(--m-space-4) 0 0;
		color: var(--m-fg-subtle);
		font-style: italic;
	}
	h3 {
		margin: 24px 0 0;
		font-size: var(--m-text-sm);
		font-weight: 600;
		color: var(--m-fg-muted);
	}
	.empty {
		display: flex;
		flex-direction: column;
		align-items: center;
		gap: var(--m-space-3);
		padding: 24px;
		color: var(--m-fg-muted);
		text-align: center;
	}
	.empty.error {
		color: var(--m-danger);
	}
	.btn {
		padding: 3px 10px;
		border: 1px solid var(--m-border);
		border-radius: var(--m-radius-sm);
		background: var(--m-bg);
		color: var(--m-fg);
		font: inherit;
		font-size: var(--m-text-xs);
		font-weight: 600;
		cursor: pointer;
	}
	.btn:hover:not(:disabled) {
		border-color: var(--m-fg-muted);
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
