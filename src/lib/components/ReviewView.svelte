<script lang="ts">
	import { onMount } from 'svelte';
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
	// user having to click into the page first.
	onMount(() => {
		scroller?.focus({ preventScroll: true });
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
