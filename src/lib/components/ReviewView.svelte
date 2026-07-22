<script lang="ts">
	import { onDestroy, onMount } from 'svelte';
	import { workspace, type SplitSide } from '../state.svelte';
	import ReviewSection from './ReviewSection.svelte';
	import { formatError, type GitStatusEntry, type ReviewComment } from '../protocol';
	import { getCachedMarkdown, renderMarkdown } from '../markdown';

	// The pane this review tab lives in. Plumbed through so a
	// section's Ctrl-click goto-def replaces the review tab in the
	// same pane (instead of jumping to whichever pane currently has
	// focus, which can differ when the user clicks across panes).
	type Props = { side: SplitSide };
	let { side }: Props = $props();

	// One pair of refs the parent owns: the scrollable container
	// (where we scroll) and a path→section element map (where
	// scroll-to-section reaches in). Bound by the child sections
	// via `registerSection` on mount/unmount.
	let scroller: HTMLDivElement | undefined = $state();
	const sectionEls = new Map<string, HTMLElement>();

	// Pull focus to the scroll container on mount so the keyboard
	// shortcuts (`n` / `p` / Alt-Arrow) start working without the
	// user having to click into the page first. Seed the workspace
	// "currently visible" pointer too: the review button's toggle
	// behaviour needs *something* to jump to even before the user
	// has scrolled, and the first entry is a sensible default.
	// Folder this review view belongs to, captured at mount. We key
	// the scroll-restore snapshot off this rather than off the live
	// active-folder pointer: on a folder switch, `onDestroy` fires
	// *after* the active folder has already flipped, so reading the
	// live pointer at teardown would stash this folder's position
	// under the next folder's state. Captured at mount, it's always
	// the folder we're actually rendering.
	const ownerFolder: string | null = workspace.activeFolderPath;

	// Index of the section we should scroll back to on mount, if a
	// restore snapshot from a previous mount survived (tab or folder
	// switch). `-1` means no restore — start at the top. Computed
	// once at mount time rather than kept reactive: it only seeds the
	// eager-mount decision and the one-shot scroll, and we don't want
	// a later git refresh that shuffles `entries` to retroactively
	// change which sections mounted eagerly.
	const restoreSnapshot = workspace.reviewRestoreFor(ownerFolder);
	const restorePath: string | null = restoreSnapshot?.path ?? null;
	const restoreOffset: number = restoreSnapshot?.offset ?? 0;
	const restoreIndex: number =
		restorePath === null ? -1 : workspace.gitStatusEntries.findIndex((e) => e.path === restorePath);

	onMount(() => {
		scroller?.focus({ preventScroll: true });
		updateVisibleFile();
		if (restoreIndex >= 0) {
			restoreScroll(restorePath as string, restoreOffset);
		}
	});

	// Re-seat the scroll position at the saved section after a tab
	// switch. Eager sections (`i <= restoreIndex`, see the `eager`
	// prop below) build their MergeView asynchronously, so the
	// target section's `offsetTop` keeps shifting as sections above
	// it grow from placeholder height to full diff height. Rather
	// than guess a single frame, we re-apply the target offset on
	// each animation frame until it stops moving (or a deadline
	// hits), which lands precisely once layout settles. The
	// deadline guards against a section that never finishes building
	// (e.g. an empty/errored diff) pinning us in a loop.
	let restoreFrame = 0;
	function cancelRestore() {
		if (restoreFrame !== 0) {
			cancelAnimationFrame(restoreFrame);
			restoreFrame = 0;
		}
	}
	function restoreScroll(path: string, offset: number) {
		const deadline = performance.now() + 1500;
		let lastTarget = -1;
		let stableFrames = 0;
		const step = () => {
			restoreFrame = 0;
			const el = sectionEls.get(path);
			if (!el || !scroller) {
				return;
			}
			const target = Math.max(0, el.offsetTop + offset);
			scroller.scrollTop = target;
			// Two consecutive frames at the same target = layout has
			// settled; stop re-applying so the user can scroll freely.
			if (target === lastTarget) {
				stableFrames += 1;
				if (stableFrames >= 2 || performance.now() >= deadline) {
					return;
				}
			} else {
				stableFrames = 0;
				lastTarget = target;
			}
			restoreFrame = requestAnimationFrame(step);
		};
		restoreFrame = requestAnimationFrame(step);
	}

	// The pointer is only meaningful while the review pane is
	// mounted. Clearing on destroy means closing the tab through
	// any other route (tab-strip close, pane teardown on folder
	// switch, …) leaves the workspace state honest.
	onDestroy(() => {
		captureRestore();
		workspace.reviewVisibleFile = null;
		workspace.reviewFocusPath = null;
		if (restoreFrame !== 0) {
			cancelAnimationFrame(restoreFrame);
			restoreFrame = 0;
		}
		// Drop any selection a child section published while we were
		// live. Sections clear their own when unmounted, but the
		// CodeMirror teardown order between us and our children
		// isn't load-bearing here — clearing again is a no-op, and
		// it guards against a section that didn't get to run its
		// cleanup (e.g. throw during destroy).
		workspace.setActiveSelection(null);
		if (scrollFrame !== 0) {
			cancelAnimationFrame(scrollFrame);
			scrollFrame = 0;
		}
	});

	// Filter ignored rows out — same vocabulary as `scmFilterPaths`.
	// In `compareBaseline === 'default'` the backend already excludes
	// ignored entries; in `'head'` mode `git status` does include
	// them under the `!! ` porcelain marker and the filter
	// suppresses them here.
	const entries: readonly GitStatusEntry[] = $derived.by(() => {
		return workspace.gitStatusEntries.filter((e) => e.status !== 'ignored');
	});
	// `mergeBase` is non-null only when the SCM panel is set to
	// the default-branch baseline. In HEAD mode the section
	// fetches the previous content via `gitHeadContent` instead;
	// `null` here is the explicit signal for "use HEAD".
	const mergeBase: string | null = $derived(
		workspace.compareBaseline === 'default' ? workspace.defaultBranchMergeBase : null,
	);
	// Banner label: "vs <branch>" in default-branch mode (matches
	// the GitHub-style PR review framing); "vs HEAD" in
	// working-tree mode (the equivalent of opening every changed
	// file's individual diff at once).
	const baselineLabel: string = $derived.by(() => {
		if (workspace.compareBaseline === 'default') {
			return shortRef(workspace.defaultBranchName);
		}
		return 'HEAD';
	});

	// File whose diff section is currently nearest the top of the
	// scroller. Surfaced in the sticky banner so the reader always
	// knows which file they're looking at, even when a tall diff
	// fills the viewport and that section's own header has scrolled
	// out of reach. Falls back to the first entry before the user
	// scrolls (seeded by `updateVisibleFile` on mount).
	const visiblePath: string | null = $derived(workspace.reviewVisibleFile);

	// Review-state summary for the banner (Phase 5.7): how many
	// unpublished comment drafts exist, and review progress across
	// the changed files. Both come from the active folder's store.
	const commentCount: number = $derived(workspace.reviewComments.length);
	// Drafts shown in the publish dialog so the user can verify each
	// comment's body + anchor before it leaves the machine. Grouped by
	// file (the review tab itself is the per-line source of truth; this
	// is the consolidated "what will go to GitHub" view the spec calls
	// for — `review-comments.md` § "Review summary").
	const publishDrafts: readonly { path: string; comments: readonly ReviewComment[] }[] = $derived.by(() => {
		const groups = new Map<string, ReviewComment[]>();
		for (const c of workspace.reviewComments) {
			const list = groups.get(c.anchor.path);
			if (list) {
				list.push(c);
			} else {
				groups.set(c.anchor.path, [c]);
			}
		}
		return [...groups.entries()].map(([path, comments]) => ({ path, comments }));
	});
	const reviewedCount: number = $derived.by(() => {
		const paths = new Set(entries.map((e) => e.path));
		return workspace.reviewedFiles.filter((r) => paths.has(r.path)).length;
	});

	// Publish dialog (Phase 5.7.2). `phase` walks the small state
	// machine: closed → form (compose summary) → busy (gh round-trip)
	// → done (show outcome). The outcome message is built from the
	// `PublishReviewResult` the backend returns.
	let publishPhase = $state<'closed' | 'form' | 'busy' | 'done'>('closed');
	let publishSummary = $state('');
	let publishOutcome = $state('');

	function openPublish() {
		publishSummary = '';
		publishOutcome = '';
		publishPhase = 'form';
	}
	function closePublish() {
		if (publishPhase !== 'busy') {
			publishPhase = 'closed';
		}
	}
	async function doPublish() {
		publishPhase = 'busy';
		try {
			const result = await workspace.publishReview(publishSummary.trim() || null);
			if (result === null) {
				publishOutcome = 'Nothing to publish.';
			} else if (result.kind === 'no_pr') {
				const branch = result.branch || 'this branch';
				publishOutcome = `No open PR for ${branch}. Push the branch and open a PR, then publish.`;
			} else if (result.posted === 0) {
				publishOutcome =
					result.lost.length > 0
						? `Couldn't place ${result.lost.length} comment${result.lost.length === 1 ? '' : 's'} on the current PR head — kept as drafts.`
						: 'Nothing was posted.';
			} else {
				const lostNote =
					result.lost.length > 0 ? ` ${result.lost.length} couldn't be placed and stayed as drafts.` : '';
				publishOutcome = `Posted ${result.posted} comment${result.posted === 1 ? '' : 's'} as one review.${lostNote}`;
			}
		} catch (err) {
			publishOutcome = `Publish failed: ${formatError(err)}`;
		}
		publishPhase = 'done';
	}

	function fileName(p: string): string {
		const slash = p.lastIndexOf('/');
		return slash === -1 ? p : p.slice(slash + 1);
	}
	// "L42" or "L10–14" — the anchor label shown in the publish preview.
	function lineLabel(c: ReviewComment): string {
		const { startLine, endLine } = c.anchor;
		return startLine === endLine ? `L${startLine}` : `L${startLine}\u2013${endLine}`;
	}
	function dirName(p: string): string {
		const slash = p.lastIndexOf('/');
		return slash === -1 ? '' : p.slice(0, slash);
	}

	// Render a comment body as markdown into the bound element. Mirrors
	// the `renderMarkdownInto` helper the inline cards use: sync cache
	// hit → innerHTML now; miss → text first, swap when the async
	// render resolves. The `.markdown-body` class is global
	// (`styles.css`) so the output looks identical to the inline card.
	function commentBody(el: HTMLElement, body: string) {
		const cached = getCachedMarkdown(body);
		if (cached !== undefined) {
			el.innerHTML = cached;
			return;
		}
		el.textContent = body;
		void renderMarkdown(body).then((html) => {
			if (el.isConnected) {
				el.innerHTML = html;
			}
		});
	}

	// Open the comment's file at its anchor line. For the "I can't find
	// this comment" case — a draft on a file no longer in the diff, or
	// anchored to an older revision — the only place to *read* it was
	// this dialog; this action makes the dialog the way back to the
	// code too. Jumps to the line in the pane the review tab lives in.
	function openCommentFile(c: ReviewComment) {
		if (publishPhase === 'busy') {
			return;
		}
		closePublish();
		void workspace.jumpTo(c.anchor.path, { line: c.anchor.startLine - 1, character: 0 }, side);
	}

	function registerSection(path: string, el: HTMLElement | null) {
		if (el === null) {
			sectionEls.delete(path);
			return;
		}
		sectionEls.set(path, el);
	}

	function scrollTo(path: string) {
		const el = sectionEls.get(path);
		if (!el) {
			return;
		}
		el.scrollIntoView({ behavior: 'smooth', block: 'start' });
	}

	function shortRef(ref: string | null): string {
		if (ref === null) {
			return '';
		}
		const slash = ref.indexOf('/');
		return slash === -1 ? ref : ref.slice(slash + 1);
	}

	// Watch the workspace's scroll-to-section signal. SCM tree clicks
	// on a row while the review tab is the active pane bump this with
	// the clicked path; we reach into our section ref map and scroll.
	// `tick` makes repeat-same-path clicks re-trigger.
	$effect(() => {
		const req = workspace.reviewScrollRequest;
		if (req === null) {
			return;
		}
		scrollTo(req.path);
	});

	// Re-evaluate the visible-file pointer when the entry list
	// changes — git refresh, baseline toggle, etc. — so the
	// review-icon's "back to file" jump never points at a row
	// the user can no longer see. Funnel through `onScroll` so
	// the rAF gate coalesces with any actual scrolling that
	// might happen in the same frame.
	$effect(() => {
		void entries;
		onScroll();
	});

	// rAF-coalesced scroll → visible-section tracker. Scroll fires
	// at every frame; without the rAF gate we'd churn the workspace
	// reactive state pointlessly. The handler also runs once on
	// mount so the pointer is set before the user touches the
	// scroller.
	let scrollFrame = 0;
	function updateVisibleFile() {
		const list = entries;
		if (list.length === 0) {
			workspace.reviewVisibleFile = null;
			return;
		}
		const idx = findNearestIndex();
		const entry = list[idx];
		if (entry !== undefined) {
			workspace.reviewVisibleFile = entry.path;
		}
	}
	function onScroll() {
		if (scrollFrame !== 0) {
			return;
		}
		scrollFrame = requestAnimationFrame(() => {
			scrollFrame = 0;
			updateVisibleFile();
		});
	}

	// Snapshot the scroll position into this folder's restore slot so
	// the next mount (after a tab *or folder* switch) lands back
	// here. We store the nearest section's path plus the signed pixel
	// offset of the scroller into that section, rather than a raw
	// `scrollTop`: section heights are recomputed from scratch on
	// remount (lazy MergeView builds), so an absolute offset would
	// point at the wrong file. Path + intra-section delta survives
	// the rebuild. Keyed off `ownerFolder` (captured at mount), not
	// the live active folder — see its declaration.
	function captureRestore() {
		// Walk the mounted section refs directly rather than the
		// reactive `entries` list: on a folder switch the git-status
		// entries may have already flipped to the new folder by the
		// time teardown runs, but `sectionEls` still holds *this*
		// folder's sections until our children unmount.
		if (!scroller || sectionEls.size === 0) {
			workspace.setReviewRestoreFor(ownerFolder, null);
			return;
		}
		const scrollTop = scroller.scrollTop;
		let bestPath: string | null = null;
		let bestEl: HTMLElement | null = null;
		let bestDelta = Infinity;
		for (const [path, el] of sectionEls) {
			const delta = Math.abs(el.offsetTop - scrollTop);
			if (delta < bestDelta) {
				bestDelta = delta;
				bestPath = path;
				bestEl = el;
			}
		}
		if (bestPath === null || bestEl === null) {
			workspace.setReviewRestoreFor(ownerFolder, null);
			return;
		}
		workspace.setReviewRestoreFor(ownerFolder, { path: bestPath, offset: scrollTop - bestEl.offsetTop });
	}

	// Keyboard nav between file sections. `n` / `p` mirror the
	// terminal-pager convention; Alt-Down / Alt-Up are the GUI
	// analogue. Bound on the scroll container so they only fire
	// while the review view has focus (the container is
	// `tabindex="0"`).
	function findNearestIndex(): number {
		if (!scroller) {
			return 0;
		}
		const scrollTop = scroller.scrollTop;
		let best = 0;
		let bestDelta = Infinity;
		const list = entries;
		for (let i = 0; i < list.length; i += 1) {
			const entry = list[i];
			if (entry === undefined) {
				continue;
			}
			const el = sectionEls.get(entry.path);
			if (!el) {
				continue;
			}
			const top = el.offsetTop;
			const delta = Math.abs(top - scrollTop);
			if (delta < bestDelta) {
				bestDelta = delta;
				best = i;
			}
		}
		return best;
	}

	function jumpRelative(dir: 1 | -1) {
		const list = entries;
		if (list.length === 0) {
			return;
		}
		const idx = findNearestIndex();
		const next = Math.min(Math.max(idx + dir, 0), list.length - 1);
		const entry = list[next];
		if (entry !== undefined) {
			scrollTo(entry.path);
		}
	}

	function onKeyDown(event: KeyboardEvent) {
		// Any genuine user interaction aborts an in-flight restore so
		// we stop yanking the viewport back to the saved position.
		cancelRestore();
		// Ignore key events that originate from within an editor:
		// CodeMirror panes are inside our scroller and capture focus
		// when the user clicks into one. Routing `n` / `p` to "next
		// file" while the user is typing into a (read-only) editor
		// search panel would break the search experience.
		const target = event.target as HTMLElement | null;
		if (target && target.closest('.cm-editor') !== null) {
			return;
		}
		if (event.key === 'n' && !event.ctrlKey && !event.metaKey && !event.altKey && !event.shiftKey) {
			event.preventDefault();
			jumpRelative(1);
			return;
		}
		if (event.key === 'p' && !event.ctrlKey && !event.metaKey && !event.altKey && !event.shiftKey) {
			event.preventDefault();
			jumpRelative(-1);
			return;
		}
		if (event.key === 'ArrowDown' && event.altKey) {
			event.preventDefault();
			jumpRelative(1);
			return;
		}
		if (event.key === 'ArrowUp' && event.altKey) {
			event.preventDefault();
			jumpRelative(-1);
		}
	}
</script>

<!-- svelte-ignore a11y_no_noninteractive_tabindex -->
<!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
<div
	class="review-view"
	bind:this={scroller}
	tabindex="0"
	role="region"
	aria-label="Review changes"
	onkeydown={onKeyDown}
	onscroll={onScroll}
	onwheel={cancelRestore}
	onpointerdown={cancelRestore}
>
	<div class="banner">
		<span class="title">Review changes</span>
		{#if baselineLabel.length > 0}
			<span class="vs">vs {baselineLabel}</span>
		{/if}
		{#if visiblePath}
			<span class="current" title={visiblePath}>
				{#if dirName(visiblePath)}<span class="dir">{dirName(visiblePath)}/</span>{/if}<span class="name"
					>{fileName(visiblePath)}</span
				>
			</span>
		{/if}
		<span class="counts">{entries.length} file{entries.length === 1 ? '' : 's'}</span>
		{#if entries.length > 0}
			<span class="progress" title="Files marked Viewed">{reviewedCount} / {entries.length} reviewed</span>
		{/if}
		{#if commentCount > 0}
			<span class="comment-count" title="Unpublished review comments">
				{commentCount} comment{commentCount === 1 ? '' : 's'}
			</span>
		{/if}
		<button
			type="button"
			class="publish"
			disabled={commentCount === 0}
			title={commentCount === 0
				? 'No review comments to publish'
				: 'Publish your review comments to the branch PR on GitHub'}
			onclick={openPublish}
		>
			Publish review →
		</button>
	</div>
	{#if entries.length === 0}
		<div class="empty">No changes against {baselineLabel.length > 0 ? baselineLabel : 'the baseline'}.</div>
	{:else}
		<div class="stack">
			<!-- Baseline is part of the key so toggling the SCM
				 panel's `vs <default>` pill remounts the sections
				 with a fresh `mergeBase` prop. Without it the
				 sections would keep rendering against the prior
				 baseline (build runs once on mount). -->
			<!-- Eager-build the first two sections (so an unscrolled
				 open shows content immediately) and, on a restore,
				 everything up to and including the section we're
				 scrolling back to — their final heights must be
				 settled before `restoreScroll` can land on the right
				 pixel. Sections below the restore target stay lazy. -->
			{#each entries as entry, i (`${entry.path}|${mergeBase ?? 'HEAD'}`)}
				<ReviewSection
					path={entry.path}
					status={entry.status}
					{mergeBase}
					eager={i < 2 || i <= restoreIndex}
					{registerSection}
					{side}
				/>
			{/each}
		</div>
	{/if}
</div>

{#if publishPhase !== 'closed'}
	<!-- svelte-ignore a11y_click_events_have_key_events -->
	<!-- svelte-ignore a11y_no_static_element_interactions -->
	<div class="pub-overlay" onclick={closePublish}>
		<!-- svelte-ignore a11y_no_static_element_interactions -->
		<div
			class="pub-card"
			role="dialog"
			aria-modal="true"
			aria-label="Publish review"
			tabindex="-1"
			onclick={(e) => e.stopPropagation()}
		>
			<header class="pub-head">
				<h2>Publish review</h2>
				<button type="button" class="pub-close" aria-label="Close" onclick={closePublish}>×</button>
			</header>

			{#if publishPhase === 'done'}
				<p class="pub-outcome">{publishOutcome}</p>
				<div class="pub-actions">
					<button type="button" class="pub-btn pub-primary" onclick={() => (publishPhase = 'closed')}> Done </button>
				</div>
			{:else}
				<p class="pub-lede">
					{commentCount} comment{commentCount === 1 ? '' : 's'} will be posted to the branch's GitHub PR as one review. Comments
					that can't be placed on the current PR head stay as local drafts.
				</p>
				<ul class="pub-drafts">
					{#each publishDrafts as group (group.path)}
						<li class="pub-draft-group">
							<div class="pub-draft-path" title={group.path}>
								{#if dirName(group.path)}<span class="dir">{dirName(group.path)}/</span>{/if}<span class="name"
									>{fileName(group.path)}</span
								>
							</div>
							<ul class="pub-draft-comments">
								{#each group.comments as c (c.id)}
									<li class="pub-draft-comment">
										<button
											type="button"
											class="pub-draft-jump"
											title="Open this file at line {c.anchor.startLine}"
											disabled={publishPhase === 'busy'}
											onclick={() => openCommentFile(c)}
										>
											<span class="pub-draft-loc">{c.anchor.side === 'base' ? 'left' : 'right'} {lineLabel(c)}</span>
										</button>
										<div class="pub-draft-body markdown-body" use:commentBody={c.body}></div>
									</li>
								{/each}
							</ul>
						</li>
					{/each}
				</ul>
				<textarea
					class="pub-summary"
					bind:value={publishSummary}
					rows="3"
					placeholder="Optional review summary…"
					disabled={publishPhase === 'busy'}
				></textarea>
				<div class="pub-actions">
					<button type="button" class="pub-btn" onclick={closePublish} disabled={publishPhase === 'busy'}>
						Cancel
					</button>
					<button type="button" class="pub-btn pub-primary" onclick={doPublish} disabled={publishPhase === 'busy'}>
						{publishPhase === 'busy' ? 'Publishing…' : 'Publish'}
					</button>
				</div>
			{/if}
		</div>
	</div>
{/if}

<style>
	.review-view {
		/* Height of the sticky banner strip. Consumed by each
		 * section's sticky header (`ReviewSection`) so a file's
		 * header parks just below the banner instead of sliding
		 * underneath it. */
		--m-review-banner-h: 35px;
		flex: 1;
		min-width: 0;
		min-height: 0;
		display: flex;
		flex-direction: column;
		overflow-y: auto;
		background: var(--m-bg);
		color: var(--m-fg);
		/* No top padding on the scroller itself: the sticky banner
		 * must park flush against the scroller's top edge, otherwise
		 * a `padding-top` would leave a strip above it where scrolled
		 * diff content peeks through. The banner and `.stack` supply
		 * their own insets instead. */
		padding: 0 12px 12px;
		gap: 12px;
		outline: none;
	}
	.banner {
		display: flex;
		align-items: baseline;
		gap: 10px;
		/* Span edge-to-edge so the sticky strip fully masks diff
		 * content sliding underneath it; the compensating padding
		 * restores the original inset for the banner's own content. */
		margin: 0 -12px;
		padding: 10px 16px 8px;
		color: var(--m-fg-muted);
		font-size: 12px;
		/* Stick to the top of the scroller so the reader always has
		 * a "you are here" file label, even when a tall diff fills
		 * the viewport and the per-section header is out of view. */
		position: sticky;
		top: 0;
		z-index: 3;
		background: var(--m-bg);
		border-bottom: 1px solid var(--m-border);
	}
	.current {
		flex: 1;
		min-width: 0;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
		font-family: var(--m-font-mono, monospace);
	}
	.current .dir {
		color: var(--m-fg-muted);
	}
	.current .name {
		color: var(--m-fg);
		font-weight: 600;
	}
	.title {
		color: var(--m-fg);
		font-weight: 600;
		font-size: 13px;
	}
	.vs {
		font-family: var(--m-font-mono, monospace);
	}
	.counts {
		margin-left: auto;
		font-variant-numeric: tabular-nums;
	}
	.progress {
		font-variant-numeric: tabular-nums;
	}
	.comment-count {
		color: var(--m-fg);
		font-variant-numeric: tabular-nums;
	}
	.publish {
		align-self: center;
		padding: 3px 10px;
		background: var(--m-accent, #4ec9b0);
		border: 1px solid var(--m-accent, #4ec9b0);
		border-radius: 4px;
		color: var(--m-bg);
		font-size: 11px;
		font-weight: 600;
		cursor: pointer;
	}
	.publish:disabled {
		opacity: 0.5;
		cursor: not-allowed;
	}
	.empty {
		padding: 24px;
		color: var(--m-fg-muted);
		text-align: center;
		font-style: italic;
	}
	.stack {
		display: flex;
		flex-direction: column;
		gap: 12px;
	}

	/* Publish dialog */
	.pub-overlay {
		position: fixed;
		inset: 0;
		background: rgba(0, 0, 0, 0.5);
		display: flex;
		align-items: center;
		justify-content: center;
		z-index: 1000;
	}
	.pub-card {
		width: min(440px, 92vw);
		background: var(--m-bg-1);
		color: var(--m-fg);
		border: 1px solid var(--m-border);
		border-radius: 10px;
		padding: 16px;
	}
	.pub-head {
		display: flex;
		justify-content: space-between;
		align-items: center;
		margin-bottom: 8px;
	}
	.pub-head h2 {
		margin: 0;
		font-size: 15px;
	}
	.pub-close {
		background: none;
		border: none;
		color: var(--m-fg-muted);
		font-size: 20px;
		line-height: 1;
		cursor: pointer;
	}
	.pub-lede,
	.pub-outcome {
		color: var(--m-fg-muted);
		font-size: 12px;
		line-height: 1.5;
		margin: 0 0 10px;
	}
	.pub-outcome {
		color: var(--m-fg);
	}
	.pub-summary {
		display: block;
		width: 100%;
		box-sizing: border-box;
		resize: vertical;
		padding: 8px;
		background: var(--m-bg);
		border: 1px solid var(--m-border);
		border-radius: 4px;
		color: var(--m-fg);
		font-family: var(--m-font-sans, system-ui, sans-serif);
		font-size: 12px;
		line-height: 1.4;
	}
	.pub-summary:focus {
		outline: none;
		border-color: var(--m-accent, #4ec9b0);
	}
	/* Per-comment preview so the user can verify each draft's body
	 * and anchor before it leaves the machine. Mirrors the banner's
	 * file-path styling (dir muted, name bold). */
	.pub-drafts {
		list-style: none;
		margin: 0 0 10px;
		padding: 0;
		max-height: 220px;
		overflow-y: auto;
		border: 1px solid var(--m-border);
		border-radius: 4px;
		background: var(--m-bg);
	}
	.pub-draft-group {
		padding: 6px 10px;
	}
	.pub-draft-group + .pub-draft-group {
		border-top: 1px solid var(--m-border);
	}
	.pub-draft-path {
		font-family: var(--m-font-mono, monospace);
		font-size: 11px;
		margin-bottom: 4px;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.pub-draft-path .dir {
		color: var(--m-fg-muted);
	}
	.pub-draft-path .name {
		color: var(--m-fg);
		font-weight: 600;
	}
	.pub-draft-comments {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: 4px;
	}
	.pub-draft-comment {
		display: flex;
		flex-direction: column;
		gap: 2px;
	}
	.pub-draft-jump {
		align-self: flex-start;
		display: inline-flex;
		align-items: baseline;
		gap: 4px;
		padding: 1px 6px;
		background: var(--m-bg);
		border: 1px solid var(--m-border);
		border-radius: 3px;
		color: var(--m-fg-muted);
		font-family: var(--m-font-mono, monospace);
		font-size: 10px;
		cursor: pointer;
		line-height: 1.4;
	}
	.pub-draft-jump:hover:not(:disabled) {
		border-color: var(--m-accent, #4ec9b0);
		color: var(--m-fg);
	}
	.pub-draft-jump:disabled {
		opacity: 0.5;
		cursor: not-allowed;
	}
	.pub-draft-loc {
		white-space: nowrap;
	}
	.pub-draft-body {
		font-size: 12px;
		line-height: 1.4;
		color: var(--m-fg);
		word-break: break-word;
	}
	:global(.pub-draft-body > :first-child) {
		margin-top: 0;
	}
	:global(.pub-draft-body > :last-child) {
		margin-bottom: 0;
	}
	.pub-actions {
		display: flex;
		justify-content: flex-end;
		gap: 8px;
		margin-top: 12px;
	}
	.pub-btn {
		padding: 5px 14px;
		background: var(--m-bg);
		border: 1px solid var(--m-border);
		border-radius: 4px;
		color: var(--m-fg);
		font-size: 12px;
		cursor: pointer;
	}
	.pub-btn:hover:not(:disabled) {
		border-color: var(--m-fg-muted);
	}
	.pub-btn:disabled {
		opacity: 0.5;
		cursor: not-allowed;
	}
	.pub-primary {
		background: var(--m-accent, #4ec9b0);
		border-color: var(--m-accent, #4ec9b0);
		color: var(--m-bg);
		font-weight: 600;
	}
</style>
