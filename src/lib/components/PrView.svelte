<script lang="ts" module>
	import type { PrOpened } from '../protocol';

	// The last answer per pull request. The tab is unmounted whenever
	// another one is shown, and re-reading takes a round trip to
	// GitHub: coming back paints this at once while the re-read runs.
	const lastOpened = new Map<string, PrOpened>();
</script>

<script lang="ts">
	// A pull request in its own tab (ADR 0113): GitHub's description,
	// checks and threads above the diff from where the branch left its
	// base. Everything is read from the repository's object store, so
	// no checkout exists and the folder the user is working in is left
	// as it is. Checking the branch out is an explicit button.
	import { onMount } from 'svelte';
	import { ipc } from '../ipc';
	import { formatError, type ReviewEvent } from '../protocol';
	import { workspace, type SplitSide } from '../state.svelte';
	import { numberFromPrPath } from '../util/prPath';
	import CommitSection from './CommitSection.svelte';
	import PullRequestPanel from './PullRequestPanel.svelte';

	type Props = { side: SplitSide };
	let { side }: Props = $props();

	const VERDICTS = [
		['comment', 'Comment'],
		['approve', 'Approve'],
		['request_changes', 'Request changes'],
	] as const;

	const FORK_TITLE = "This pull request's branch lives in a fork, so origin has no branch to check out.";

	let scroller: HTMLDivElement | undefined = $state();
	const sectionEls = new Map<string, HTMLElement>();

	const number = $derived.by(() => {
		const path = side === 'left' ? workspace.leftActive : workspace.rightActive;
		return path === null ? null : numberFromPrPath(path);
	});
	const cacheKey = $derived(`${workspace.activeProjectPath ?? ''}\n${number ?? ''}`);

	let opened = $state<PrOpened | null>(null);
	let loadError = $state('');
	let loading = $state(false);
	let loadToken = 0;

	const detail = $derived(opened?.detail ?? null);
	const files = $derived(opened?.files ?? []);
	const paths = $derived(new Set(files.map((file) => file.path)));

	async function load() {
		const wanted = number;
		if (wanted === null) {
			loadError = 'Not a pull request.';
			return;
		}
		const key = cacheKey;
		const token = ++loadToken;
		loading = true;
		try {
			const result = await ipc.pullRequest.open(wanted);
			if (token !== loadToken) {
				return;
			}
			lastOpened.set(key, result);
			opened = result;
			loadError = '';
		} catch (err) {
			if (token !== loadToken) {
				return;
			}
			// A failed re-read keeps what is on screen.
			if (opened === null) {
				loadError = formatError(err);
			} else {
				workspace.flash(`Could not re-read the pull request: ${formatError(err)}`);
			}
		} finally {
			if (token === loadToken) {
				loading = false;
			}
		}
	}

	onMount(() => {
		scroller?.focus({ preventScroll: true });
		opened = lastOpened.get(cacheKey) ?? null;
		void load();
		return () => {
			loadToken++;
		};
	});

	function registerSection(path: string, el: HTMLElement | null) {
		if (el === null) {
			sectionEls.delete(path);
			return;
		}
		sectionEls.set(path, el);
	}

	function scrollTo(path: string) {
		sectionEls.get(path)?.scrollIntoView({ behavior: 'smooth', block: 'start' });
	}

	// Review dialog: closed → form → busy (the GitHub round trip) →
	// done (the outcome). Verdict and summary only; the tab has no
	// draft comments.
	let reviewPhase = $state<'closed' | 'form' | 'busy' | 'done'>('closed');
	let reviewEvent = $state<ReviewEvent>('comment');
	let reviewSummary = $state('');
	let reviewOutcome = $state('');
	const reviewBlocked = $derived(reviewEvent !== 'approve' && reviewSummary.trim() === '');

	function openReview() {
		reviewEvent = 'comment';
		reviewSummary = '';
		reviewOutcome = '';
		reviewPhase = 'form';
	}

	function closeReview() {
		if (reviewPhase !== 'busy') {
			reviewPhase = 'closed';
		}
	}

	async function postReview() {
		const current = detail;
		if (current === null || reviewBlocked) {
			return;
		}
		reviewPhase = 'busy';
		const event = reviewEvent;
		try {
			const result = await ipc.fs.publishPrReview({
				body: reviewSummary.trim() || null,
				comments: [],
				event,
				prNumber: current.number,
			});
			if (result.kind === 'no_pr') {
				reviewOutcome = `Pull request #${current.number} could not be found on the remote.`;
			} else if (result.reviewUrl === '') {
				reviewOutcome = 'Nothing was posted.';
			} else {
				reviewOutcome =
					event === 'approve' ? 'Approved.' : event === 'request_changes' ? 'Requested changes.' : 'Posted the review.';
				void load();
			}
		} catch (err) {
			reviewOutcome = `Posting failed: ${formatError(err)}`;
		}
		reviewPhase = 'done';
	}

	function focusSummary(el: HTMLTextAreaElement) {
		el.focus();
	}

	function onDialogKeydown(event: KeyboardEvent) {
		if (event.key === 'Escape') {
			event.stopPropagation();
			closeReview();
		}
	}

	let checkingOut = $state(false);
	async function checkOut() {
		const project = workspace.activeProjectPath;
		if (detail === null || project === null || checkingOut) {
			return;
		}
		checkingOut = true;
		try {
			await workspace.openPrForReview(project, detail.headRef);
		} finally {
			checkingOut = false;
		}
	}

	let startingAgent = $state(false);
	async function startAgent() {
		if (detail === null || startingAgent) {
			return;
		}
		startingAgent = true;
		try {
			await workspace.newCoderWorktreeSession(detail.headRef);
		} finally {
			startingAgent = false;
		}
	}
</script>

<!-- svelte-ignore a11y_no_noninteractive_tabindex -->
<div class="pr-view" bind:this={scroller} tabindex="0" role="region" aria-label="Pull request">
	{#if opened === null || detail === null}
		<div class="empty" class:error={loadError !== ''}>
			{#if loadError !== ''}
				Could not open the pull request: {loadError}
				<button type="button" class="btn" onclick={() => void load()} disabled={loading}>Try again</button>
			{:else}
				Loading pull request…
			{/if}
		</div>
	{:else}
		<div class="banner">
			<span class="title">#{detail.number} {detail.title}</span>
			<span class="counts">{files.length} file{files.length === 1 ? '' : 's'}</span>
			<button
				type="button"
				class="btn"
				disabled={detail.isCrossRepository || checkingOut}
				title={detail.isCrossRepository
					? FORK_TITLE
					: `Check ${detail.headRef} out in a worktree and review it there, with drafts and a terminal`}
				onclick={() => void checkOut()}
			>
				{checkingOut ? 'Checking out…' : 'Check out in a worktree'}
			</button>
			<button
				type="button"
				class="btn"
				disabled={detail.isCrossRepository || startingAgent}
				title={detail.isCrossRepository
					? FORK_TITLE
					: `Start a coder session on ${detail.headRef}, in its own worktree`}
				onclick={() => void startAgent()}
			>
				{startingAgent ? 'Starting…' : 'Start an agent on it'}
			</button>
			<button type="button" class="btn primary" title="Comment on, approve or request changes" onclick={openReview}>
				Review
			</button>
		</div>
		<PullRequestPanel pr={detail} {paths} refreshing={loading} onJump={scrollTo} onRefresh={() => void load()} />
		{#if files.length === 0}
			<div class="empty">No files differ between {detail.headRef} and where it left {detail.baseRef}.</div>
		{:else}
			<div class="stack">
				{#each files as entry, i (`${entry.path}|${opened.mergeBase}|${detail.headSha}`)}
					<CommitSection
						path={entry.path}
						status={entry.status}
						commitSha={detail.headSha}
						parentSha={opened.mergeBase}
						eager={i < 2}
						pullRequest={detail}
						{registerSection}
						{side}
					/>
				{/each}
			</div>
		{/if}
	{/if}
</div>

{#if reviewPhase !== 'closed' && detail !== null}
	<!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions (Escape is handled on the dialog) -->
	<div class="overlay" onclick={closeReview}>
		<div
			class="card"
			role="dialog"
			aria-modal="true"
			aria-label="Review pull request"
			tabindex="-1"
			onclick={(event) => event.stopPropagation()}
			onkeydown={onDialogKeydown}
		>
			<header class="card-head">
				<h2>Review #{detail.number}</h2>
				<button type="button" class="close" aria-label="Close" onclick={closeReview}>×</button>
			</header>
			{#if reviewPhase === 'done'}
				<p class="outcome">{reviewOutcome}</p>
				<div class="actions">
					<button type="button" class="btn primary" onclick={() => (reviewPhase = 'closed')}>Done</button>
				</div>
			{:else}
				<p class="lede">Posts a verdict, or a summary alone, on the pull request. Nothing is attached to a line.</p>
				<div class="events" role="radiogroup" aria-label="Review verdict">
					{#each VERDICTS as [value, label] (value)}
						<label class="event" class:selected={reviewEvent === value}>
							<input
								type="radio"
								name="pr-review-event"
								{value}
								bind:group={reviewEvent}
								disabled={reviewPhase === 'busy'}
							/>
							{label}
						</label>
					{/each}
				</div>
				<textarea
					class="summary"
					bind:value={reviewSummary}
					rows="4"
					{@attach focusSummary}
					placeholder={reviewEvent === 'approve'
						? 'Optional summary…'
						: reviewEvent === 'request_changes'
							? 'What needs to change…'
							: 'Your comment…'}
					disabled={reviewPhase === 'busy'}
				></textarea>
				<div class="actions">
					<button type="button" class="btn" onclick={closeReview} disabled={reviewPhase === 'busy'}>Cancel</button>
					<button
						type="button"
						class="btn primary"
						onclick={() => void postReview()}
						disabled={reviewPhase === 'busy' || reviewBlocked}
						title={reviewBlocked ? 'Write a summary first' : undefined}
					>
						{reviewPhase === 'busy'
							? 'Posting…'
							: reviewEvent === 'approve'
								? 'Approve'
								: reviewEvent === 'request_changes'
									? 'Request changes'
									: 'Comment'}
					</button>
				</div>
			{/if}
		</div>
	</div>
{/if}

<style>
	.pr-view {
		/* Read by each section's sticky header so it parks under the
		 * banner instead of sliding beneath it. */
		--m-review-banner-h: 37px;
		flex: 1;
		min-width: 0;
		min-height: 0;
		display: flex;
		flex-direction: column;
		overflow-y: auto;
		background: var(--m-bg);
		color: var(--m-fg);
		padding: 0 var(--m-space-3) var(--m-space-3);
		outline: none;
	}
	.banner {
		display: flex;
		align-items: center;
		gap: var(--m-space-2);
		box-sizing: border-box;
		height: var(--m-review-banner-h);
		flex-shrink: 0;
		margin: 0 calc(-1 * var(--m-space-3));
		padding: 0 var(--m-space-4);
		position: sticky;
		top: 0;
		z-index: 3;
		background: var(--m-bg);
		border-bottom: 1px solid var(--m-border);
		font-size: var(--m-text-sm);
		color: var(--m-fg-muted);
	}
	.title {
		flex: 1;
		min-width: 0;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
		color: var(--m-fg);
		font-size: var(--m-text-md);
		font-weight: 600;
	}
	.counts {
		flex-shrink: 0;
		font-variant-numeric: tabular-nums;
	}
	/* The panel brings its own side margins for the review tab's
	 * narrower gutter; here the scroller already has them. */
	.pr-view :global(section.pr) {
		margin: var(--m-space-3) 0;
	}
	.stack {
		display: flex;
		flex-direction: column;
		gap: var(--m-space-3);
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
		flex-shrink: 0;
		padding: 3px 10px;
		border: 1px solid var(--m-border);
		border-radius: var(--m-radius-sm);
		background: var(--m-bg);
		color: var(--m-fg);
		font: inherit;
		font-size: var(--m-text-xs);
		font-weight: 600;
		white-space: nowrap;
		cursor: pointer;
	}
	.btn:hover:not(:disabled) {
		border-color: var(--m-fg-muted);
	}
	.btn:disabled {
		opacity: 0.5;
		cursor: not-allowed;
	}
	.btn.primary {
		background: var(--m-accent);
		border-color: var(--m-accent);
		color: var(--m-bg);
	}

	.overlay {
		position: fixed;
		inset: 0;
		z-index: 1000;
		display: flex;
		align-items: center;
		justify-content: center;
		background: rgba(0, 0, 0, 0.5);
	}
	.card {
		width: min(440px, 92vw);
		padding: var(--m-space-4);
		border: 1px solid var(--m-border);
		border-radius: var(--m-radius-lg);
		background: var(--m-bg-1);
		color: var(--m-fg);
		box-shadow: var(--m-shadow-pop);
		outline: none;
	}
	.card-head {
		display: flex;
		align-items: center;
		justify-content: space-between;
		margin-bottom: var(--m-space-2);
	}
	.card-head h2 {
		margin: 0;
		font-size: var(--m-text-lg);
	}
	.close {
		border: none;
		background: none;
		color: var(--m-fg-muted);
		font-size: 20px;
		line-height: 1;
		cursor: pointer;
	}
	.lede,
	.outcome {
		margin: 0 0 var(--m-space-3);
		color: var(--m-fg-muted);
		font-size: var(--m-text-sm);
		line-height: 1.5;
	}
	.outcome {
		color: var(--m-fg);
	}
	.events {
		display: flex;
		gap: var(--m-space-2);
		margin-bottom: var(--m-space-2);
	}
	.event {
		display: inline-flex;
		align-items: center;
		gap: var(--m-space-1);
		padding: 3px 10px;
		border: 1px solid var(--m-border);
		border-radius: var(--m-radius-pill);
		color: var(--m-fg-muted);
		font-size: var(--m-text-sm);
		cursor: pointer;
	}
	.event.selected {
		border-color: var(--m-accent);
		color: var(--m-fg);
	}
	.event input {
		margin: 0;
	}
	.summary {
		display: block;
		width: 100%;
		box-sizing: border-box;
		resize: vertical;
		padding: var(--m-space-2);
		border: 1px solid var(--m-border);
		border-radius: var(--m-radius-sm);
		background: var(--m-bg);
		color: var(--m-fg);
		font-family: var(--m-font-ui);
		font-size: var(--m-text-sm);
		line-height: 1.4;
	}
	.summary:focus {
		outline: none;
		border-color: var(--m-accent);
	}
	.actions {
		display: flex;
		justify-content: flex-end;
		gap: var(--m-space-2);
		margin-top: var(--m-space-3);
	}
	.actions .btn {
		padding: 5px 14px;
		font-size: var(--m-text-sm);
	}
</style>
