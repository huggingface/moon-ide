<script lang="ts" module>
	import type { PrDiff, PrDiffFile, PullRequestDetail } from '../protocol';

	// The last answer per pull request. The tab is unmounted whenever
	// another one is shown, and re-reading takes round trips to GitHub
	// and to the remote: coming back paints this at once while the
	// re-read runs.
	type Loaded = { detail: PullRequestDetail; diff: PrDiff | null };
	const lastLoaded = new Map<string, Loaded>();

	// Viewed marks per pull request, for as long as the IDE runs. They
	// belong to one head commit: a push may have changed any file, so
	// it clears them all.
	const viewedMarks = new Map<string, { headSha: string; paths: Record<string, boolean> }>();

	// The file each pull request's tab was last on, to come back to.
	const lastPlace = new Map<string, string>();
</script>

<script lang="ts">
	// A pull request in its own tab (ADR 0115): GitHub's description,
	// conversation, checks and threads above the diff from where the
	// branch left its base. Everything is read from the repository's
	// object store, so no checkout exists and the folder the user is
	// working in is left as it is. Checking the branch out is an
	// explicit button.
	//
	// It loads in stages so there is something to read early: the
	// description from one `gh` call, then the review threads and the
	// diff (a fetch from the remote, the slow part) side by side.
	//
	// Files are reviewed one at a time rather than scrolled through
	// (ADR 0118): a list of them beside the diffs, a viewed mark that
	// folds a file away and opens the next, and nothing open to begin
	// with when there are many.
	import { onMount, tick } from 'svelte';
	import {
		attentionCounts,
		LARGE_PR_FILES,
		nextUnviewed,
		reviewSize,
		startsCollapsed,
		type FileOutline,
	} from '../prFiles';
	import { ipc } from '../ipc';
	import { formatError, type ReviewEvent } from '../protocol';
	import { workspace, type SplitSide } from '../state.svelte';
	import { statusColor } from '../fileIcon';
	import { parsePrPath } from '../util/prPath';
	import CommitSection from './CommitSection.svelte';
	import DiffStat from './DiffStat.svelte';
	import FileIcon from './FileIcon.svelte';
	import TabTitle from './TabTitle.svelte';
	import PullRequestPanel from './PullRequestPanel.svelte';
	import CheckIcon from './icons/CheckIcon.svelte';

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

	const tabPath = $derived(side === 'left' ? workspace.leftActive : workspace.rightActive);
	const ref = $derived(tabPath === null ? null : parsePrPath(tabPath));
	const number = $derived(ref?.number ?? null);
	// The project the pull request is in. The tab is shown wherever
	// the user is (ADR 0121), so this is not the active folder.
	const folder = $derived(ref?.folder ?? null);
	const elsewhere = $derived(folder !== null && workspace.activeProjectPath !== folder);
	// What the tab strip already knows, to head the page before
	// GitHub answers.
	const tabName = $derived(workspace.openFiles.find((file) => file.path === tabPath)?.name ?? '');
	const cacheKey = $derived(tabPath ?? '');

	let detail = $state<PullRequestDetail | null>(null);
	let diff = $state<PrDiff | null>(null);
	let loadError = $state('');
	let diffError = $state('');
	let loading = $state(false);
	let loadToken = 0;

	const files = $derived(diff?.files ?? []);
	const paths = $derived(new Set(files.map((file) => file.path)));

	let viewed = $state<Record<string, boolean>>({});
	let viewedSha = '';
	// The reader's own open / closed choices, over `startsCollapsed`.
	let folded = $state<Record<string, boolean>>({});
	const attention = $derived(detail === null ? new Map<string, number>() : attentionCounts(detail));
	const viewedCount = $derived(files.filter((file) => viewed[file.path]).length);
	const size = $derived(reviewSize(files));

	function isCollapsed(file: PrDiffFile): boolean {
		return folded[file.path] ?? (viewed[file.path] === true || startsCollapsed(file, files.length));
	}

	/** Take up the marks made for `headSha` earlier, or start clean. */
	function adoptViewed(headSha: string) {
		if (headSha === viewedSha) {
			return;
		}
		viewedSha = headSha;
		const marks = viewedMarks.get(cacheKey);
		viewed = marks?.headSha === headSha ? { ...marks.paths } : {};
		folded = {};
	}

	function setViewed(path: string, value: boolean) {
		viewed[path] = value;
		folded[path] = value;
		viewedMarks.set(cacheKey, { headSha: viewedSha, paths: $state.snapshot(viewed) });
		if (!value) {
			return;
		}
		const next = nextUnviewed(files, viewed, path);
		if (next !== null) {
			void jumpTo(next);
		}
	}

	function toggleFold(file: PrDiffFile) {
		folded[file.path] = !isCollapsed(file);
	}

	function outlineFor(file: PrDiffFile): FileOutline {
		return {
			additions: file.additions,
			deletions: file.deletions,
			isTest: file.isTest,
			attention: attention.get(file.path) ?? 0,
			collapsed: isCollapsed(file),
			viewed: viewed[file.path] === true,
			onToggle: () => toggleFold(file),
			onViewed: (value) => setViewed(file.path, value),
		};
	}

	// The file under the top of the viewport; `null` above the first
	// one, where the description is.
	let activePath = $state<string | null>(null);
	let spyFrame = -1;

	function setActive(path: string | null) {
		activePath = path;
		if (path === null) {
			lastPlace.delete(cacheKey);
		} else {
			lastPlace.set(cacheKey, path);
		}
	}

	function spy() {
		cancelAnimationFrame(spyFrame);
		spyFrame = requestAnimationFrame(() => {
			if (scroller === undefined) {
				return;
			}
			const line = scroller.getBoundingClientRect().top + BANNER_PX + 12;
			let found: string | null = null;
			for (const file of files) {
				const el = sectionEls.get(file.path);
				if (el === undefined || el.getBoundingClientRect().top > line) {
					break;
				}
				found = file.path;
			}
			setActive(found);
		});
	}

	// Wide enough for the list to sit beside two diff columns; below
	// that it opens over the page, from the banner.
	const DOCK_PX = 1000;
	const BANNER_PX = 37;
	let width = $state(0);
	let railWanted = $state<boolean | null>(null);
	const docked = $derived(width >= DOCK_PX);
	const railShown = $derived(files.length > 1 && (railWanted ?? docked));

	let railList: HTMLDivElement | undefined = $state();
	// Keeps the current file in sight in a list longer than the pane.
	$effect(() => {
		void activePath;
		void tick().then(() => railList?.querySelector('.rail-item.active')?.scrollIntoView({ block: 'nearest' }));
	});

	function leaveRail() {
		if (!docked) {
			railWanted = null;
		}
	}

	function toOverview() {
		leaveRail();
		scroller?.scrollTo({ top: 0 });
	}

	/** Open `path`'s diff and bring it under the banner. In a large
	 *  pull request the other files close, so moving through them
	 *  leaves one diff on the page rather than a growing pile. */
	async function jumpTo(path: string) {
		leaveRail();
		if (files.length > LARGE_PR_FILES) {
			for (const file of files) {
				folded[file.path] = file.path !== path;
			}
		} else {
			folded[path] = false;
		}
		await tick();
		sectionEls.get(path)?.scrollIntoView({ block: 'start' });
		setActive(path);
	}

	function step(delta: 1 | -1) {
		const at = files.findIndex((file) => file.path === activePath);
		const next = files[at === -1 ? (delta === 1 ? 0 : -1) : at + delta];
		if (next !== undefined) {
			void jumpTo(next.path);
		}
	}

	function onKeydown(event: KeyboardEvent) {
		if (event.ctrlKey || event.metaKey || event.altKey || files.length === 0) {
			return;
		}
		const target = event.target;
		const typing =
			target instanceof HTMLTextAreaElement ||
			target instanceof HTMLSelectElement ||
			(target instanceof HTMLInputElement && target.type !== 'checkbox');
		if (typing) {
			return;
		}
		if (event.key === 'j' || event.key === 'k') {
			event.preventDefault();
			step(event.key === 'j' ? 1 : -1);
			return;
		}
		if (event.key === 'v' && activePath !== null) {
			event.preventDefault();
			setViewed(activePath, viewed[activePath] !== true);
		}
	}

	function baseName(path: string): string {
		return path.slice(path.lastIndexOf('/') + 1);
	}

	function dirName(path: string): string {
		const cut = path.lastIndexOf('/');
		return cut === -1 ? '' : path.slice(0, cut);
	}

	/** Re-read the pull request. Resolves once the detail is in (or
	 *  has failed); the threads and the diff follow by themselves. */
	async function load() {
		const wanted = ref;
		if (wanted === null) {
			loadError = 'Not a pull request.';
			return;
		}
		const key = cacheKey;
		const token = ++loadToken;
		const current = () => token === loadToken;
		loading = true;
		let fresh: PullRequestDetail;
		try {
			fresh = await ipc.pullRequest.detail(wanted.folder, wanted.number);
		} catch (err) {
			if (!current()) {
				return;
			}
			loading = false;
			// A failed re-read keeps what is on screen.
			if (detail === null) {
				loadError = formatError(err);
			} else {
				workspace.flash(`Could not re-read the pull request: ${formatError(err)}`);
			}
			return;
		}
		if (!current()) {
			return;
		}
		// The threads arrive separately; until they do, keep the
		// ones on screen instead of blinking them away.
		detail = { ...fresh, threads: detail?.threads ?? [] };
		adoptViewed(fresh.headSha);
		loadError = '';
		void loadRest(wanted.folder, fresh, key, current);
	}

	async function loadRest(project: string, fresh: PullRequestDetail, key: string, current: () => boolean) {
		const threads = ipc.pullRequest.threads(fresh.url).then(
			(result) => {
				if (current() && detail !== null) {
					detail = { ...detail, threads: result };
				}
			},
			() => {},
		);
		const diffed = ipc.pullRequest.diff(project, fresh.number, fresh.baseRef, fresh.headSha).then(
			(result) => {
				if (current()) {
					diff = result;
					diffError = '';
				}
			},
			(err: unknown) => {
				if (current()) {
					diffError = formatError(err);
				}
			},
		);
		await Promise.all([threads, diffed]);
		if (!current()) {
			return;
		}
		loading = false;
		if (detail !== null) {
			lastLoaded.set(key, { detail, diff });
		}
	}

	onMount(() => {
		scroller?.focus({ preventScroll: true });
		const last = lastLoaded.get(cacheKey);
		if (last !== undefined) {
			detail = last.detail;
			diff = last.diff;
			adoptViewed(last.detail.headSha);
			const place = lastPlace.get(cacheKey);
			if (place !== undefined && last.diff?.files.some((file) => file.path === place)) {
				void jumpTo(place);
			}
		}
		void load();
		return () => {
			loadToken++;
			cancelAnimationFrame(spyFrame);
		};
	});

	function registerSection(path: string, el: HTMLElement | null) {
		if (el === null) {
			sectionEls.delete(path);
			return;
		}
		sectionEls.set(path, el);
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
			const result = await ipc.fs.publishPrReview(
				{
					body: reviewSummary.trim() || null,
					comments: [],
					event,
					prNumber: current.number,
				},
				folder ?? undefined,
			);
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
		if (detail === null || folder === null || checkingOut) {
			return;
		}
		checkingOut = true;
		try {
			await workspace.openPrForReview(folder, detail.headRef);
		} finally {
			checkingOut = false;
		}
	}

	let startingAgent = $state(false);
	async function startAgent() {
		if (detail === null || folder === null || startingAgent) {
			return;
		}
		startingAgent = true;
		try {
			// The session is made in the pull request's project, which
			// becomes the active one; this tab comes along.
			if (workspace.activeProjectPath !== folder) {
				await workspace.setActiveFolder(folder);
				if (workspace.activeProjectPath !== folder) {
					return;
				}
			}
			await workspace.newCoderWorktreeSession(detail.headRef);
		} finally {
			startingAgent = false;
		}
	}
</script>

<!-- svelte-ignore a11y_no_static_element_interactions (j / k / v move through the files; every control they reach is also a button) -->
<div class="pr-view" bind:clientWidth={width} onkeydown={onKeydown}>
	{#if railShown && detail !== null}
		<nav class="rail" class:floating={!docked} aria-label="Files changed">
			<div class="rail-head">
				<span>{viewedCount} / {files.length} viewed</span>
				<DiffStat
					additions={size.additions}
					deletions={size.deletions}
					tests={{ additions: size.testAdditions, deletions: size.testDeletions }}
				/>
			</div>
			<div class="rail-list" bind:this={railList}>
				<button type="button" class="rail-item" class:active={activePath === null} onclick={toOverview}>
					<span class="rail-name">Overview</span>
				</button>
				{#each files as file (file.path)}
					<button
						type="button"
						class="rail-item"
						class:active={activePath === file.path}
						class:viewed={viewed[file.path]}
						title={`${file.path} (${file.status})`}
						onclick={() => void jumpTo(file.path)}
					>
						<FileIcon path={file.path} size={14} />
						<span
							class="rail-name"
							class:gone={file.status === 'deleted'}
							style:color={viewed[file.path] ? undefined : statusColor(file.status)}
						>
							{baseName(file.path)}
						</span>
						<span class="rail-dir">{dirName(file.path)}</span>
						{#if (attention.get(file.path) ?? 0) > 0}
							<span class="rail-attention">{attention.get(file.path)}</span>
						{/if}
						{#if viewed[file.path]}
							<span class="rail-check"><CheckIcon size={12} /></span>
						{:else}
							<DiffStat additions={file.additions} deletions={file.deletions} muted={file.isTest} />
						{/if}
					</button>
				{/each}
			</div>
		</nav>
	{/if}
	<!-- svelte-ignore a11y_no_noninteractive_tabindex -->
	<div class="main" bind:this={scroller} tabindex="0" role="region" aria-label="Pull request" onscroll={spy}>
		{#if detail === null}
			<div class="banner">
				<span class="title"><TabTitle name={tabName || (number === null ? 'Pull request' : `#${number}`)} /></span>
			</div>
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
				<span class="title"><TabTitle name={`#${detail.number} ${detail.title}`} /></span>
				{#if elsewhere && folder !== null}
					<span class="project" title={`A pull request of ${folder}, not of the folder you are in`}>
						{baseName(folder)}
					</span>
				{/if}
				{#if files.length > 1}
					<button
						type="button"
						class="btn"
						aria-pressed={railShown}
						title="List of changed files (j / k next and previous, v viewed)"
						onclick={() => (railWanted = !railShown)}
					>
						Files {viewedCount}/{files.length}
					</button>
				{:else if diff !== null}
					<span class="counts">{files.length} file{files.length === 1 ? '' : 's'}</span>
				{/if}
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
			<PullRequestPanel
				pr={detail}
				{paths}
				refreshing={loading}
				onJump={(path) => void jumpTo(path)}
				onRefresh={load}
			/>
			{#if diff === null}
				<div class="empty" class:error={diffError !== ''}>
					{#if diffError !== ''}
						Could not load the diff: {diffError}
						<button type="button" class="btn" onclick={() => void load()} disabled={loading}>Try again</button>
					{:else}
						Fetching the diff…
					{/if}
				</div>
			{:else if files.length === 0}
				<div class="empty">No files differ between {detail.headRef} and where it left {detail.baseRef}.</div>
			{:else}
				<div class="stack">
					{#each files as entry, i (`${entry.path}|${diff.mergeBase}|${detail.headSha}`)}
						<CommitSection
							path={entry.path}
							status={entry.status}
							commitSha={detail.headSha}
							parentSha={diff.mergeBase}
							eager={i < 2}
							pullRequest={detail}
							outline={outlineFor(entry)}
							folderPath={folder ?? undefined}
							{registerSection}
							{side}
						/>
					{/each}
				</div>
				{#if files.length > 1}
					<div class="tail"></div>
				{/if}
			{/if}
		{/if}
	</div>
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
		position: relative;
		flex: 1;
		min-width: 0;
		min-height: 0;
		display: flex;
		background: var(--m-bg);
		color: var(--m-fg);
	}
	.rail {
		display: flex;
		flex-direction: column;
		flex-shrink: 0;
		width: 232px;
		min-height: 0;
		border-right: 1px solid var(--m-border);
		background: var(--m-bg-1);
		font-size: var(--m-text-sm);
	}
	.rail.floating {
		position: absolute;
		top: 37px;
		bottom: 0;
		left: 0;
		z-index: 4;
		width: min(320px, 90%);
		box-shadow: var(--m-shadow-pop);
	}
	.rail-head {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: var(--m-space-2);
		box-sizing: border-box;
		height: 37px;
		flex-shrink: 0;
		padding: 0 var(--m-space-3);
		border-bottom: 1px solid var(--m-border);
		color: var(--m-fg-muted);
		font-variant-numeric: tabular-nums;
	}
	.rail-list {
		flex: 1;
		min-height: 0;
		overflow-y: auto;
		padding: var(--m-space-1);
	}
	.rail-item {
		display: flex;
		align-items: center;
		gap: 6px;
		width: 100%;
		height: 26px;
		padding: 0 var(--m-space-2);
		border: none;
		border-radius: var(--m-radius-sm);
		background: transparent;
		color: var(--m-fg);
		font: inherit;
		text-align: left;
		cursor: pointer;
	}
	.rail-item:hover {
		background: var(--m-bg-2);
	}
	.rail-item.active {
		background: var(--m-bg-3);
	}
	.rail-item.viewed .rail-name,
	.rail-item.viewed .rail-dir {
		color: var(--m-fg-subtle);
	}
	.rail-item.viewed :global(.file-icon) {
		opacity: 0.45;
	}
	.rail-name.gone {
		text-decoration: line-through;
	}
	.rail-name {
		flex-shrink: 0;
		max-width: 60%;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.rail-dir {
		flex: 1;
		min-width: 0;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
		color: var(--m-fg-subtle);
		font-size: var(--m-text-xs);
	}
	.rail-attention {
		flex-shrink: 0;
		min-width: 16px;
		padding: 0 5px;
		border-radius: var(--m-radius-pill);
		background: color-mix(in srgb, var(--m-accent) 16%, transparent);
		color: var(--m-accent);
		font-size: var(--m-text-xs);
		line-height: 16px;
		text-align: center;
	}
	.rail-check {
		display: inline-flex;
		flex-shrink: 0;
		color: var(--m-success);
	}
	.main {
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
	.project {
		flex-shrink: 0;
		padding: 0 var(--m-space-2);
		border: 1px solid var(--m-border);
		border-radius: var(--m-radius-pill);
		color: var(--m-fg-muted);
		font-size: var(--m-text-xs);
		line-height: 18px;
	}
	.counts {
		flex-shrink: 0;
		font-variant-numeric: tabular-nums;
	}
	/* The panel brings its own side margins for the review tab's
	 * narrower gutter; here the scroller already has them. */
	.main :global(section.pr) {
		margin: var(--m-space-3) 0;
	}
	/* Room below the last file, so any of them can be brought up
	 * under the banner. */
	.tail {
		flex-shrink: 0;
		height: 100vh;
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
