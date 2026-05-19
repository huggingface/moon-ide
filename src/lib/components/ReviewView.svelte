<script lang="ts">
	import { onDestroy, onMount } from 'svelte';
	import { workspace } from '../state.svelte';
	import ReviewSection from './ReviewSection.svelte';
	import type { GitStatusEntry } from '../protocol';

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
	onMount(() => {
		scroller?.focus({ preventScroll: true });
		updateVisibleFile();
	});

	// The pointer is only meaningful while the review pane is
	// mounted. Clearing on destroy means closing the tab through
	// any other route (tab-strip close, pane teardown on folder
	// switch, …) leaves the workspace state honest.
	onDestroy(() => {
		workspace.reviewVisibleFile = null;
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
>
	<div class="banner">
		<span class="title">Review changes</span>
		{#if baselineLabel.length > 0}
			<span class="vs">vs {baselineLabel}</span>
		{/if}
		<span class="counts">{entries.length} file{entries.length === 1 ? '' : 's'}</span>
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
			{#each entries as entry, i (`${entry.path}|${mergeBase ?? 'HEAD'}`)}
				<ReviewSection path={entry.path} status={entry.status} {mergeBase} eager={i < 2} {registerSection} />
			{/each}
		</div>
	{/if}
</div>

<style>
	.review-view {
		flex: 1;
		min-width: 0;
		min-height: 0;
		display: flex;
		flex-direction: column;
		overflow-y: auto;
		background: var(--m-bg);
		color: var(--m-fg);
		padding: 12px;
		gap: 12px;
		outline: none;
	}
	.banner {
		display: flex;
		align-items: baseline;
		gap: 10px;
		padding: 2px 4px;
		color: var(--m-fg-muted);
		font-size: 12px;
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
</style>
