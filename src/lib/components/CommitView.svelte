<script lang="ts">
	import { onMount, onDestroy } from 'svelte';
	import { workspace, type SplitSide } from '../state.svelte';
	import { ipc } from '../ipc';
	import { shaFromCommitPath } from '../util/commitPath';
	import { formatError, type CommitDiff } from '../protocol';
	import CommitSection from './CommitSection.svelte';

	type Props = { side: SplitSide };
	let { side }: Props = $props();

	let scroller: HTMLDivElement | undefined = $state();
	const sectionEls = new Map<string, HTMLElement>();

	// Resolve the SHA from the active path so each commit gets its
	// own tab. The path is `commit://<sha>`; `shaFromCommitPath`
	// validates the 40-char hex shape.
	const sha = $derived.by(() => {
		const path = side === 'left' ? workspace.leftActive : workspace.rightActive;
		return path !== null ? shaFromCommitPath(path) : null;
	});

	let diff = $state<CommitDiff | null>(null);
	let loadError = $state('');

	onMount(() => {
		scroller?.focus({ preventScroll: true });
		void loadDiff();
	});

	onDestroy(() => {
		// Nothing to restore — commit views are ephemeral.
	});

	async function loadDiff() {
		const resolvedSha = sha;
		if (resolvedSha === null) {
			return;
		}
		try {
			const result = await ipc.fs.gitCommitDiff(resolvedSha);
			diff = result;
			if (result === null) {
				loadError = 'Commit not found.';
			}
		} catch (err) {
			loadError = formatError(err);
		}
	}

	// Re-load when the SHA changes (switching between commit tabs).
	$effect(() => {
		void sha;
		void loadDiff();
	});

	function registerSection(path: string, el: HTMLElement | null) {
		if (el === null) {
			sectionEls.delete(path);
			return;
		}
		sectionEls.set(path, el);
	}

	function fileName(p: string): string {
		const slash = p.lastIndexOf('/');
		return slash === -1 ? p : p.slice(slash + 1);
	}
	function dirName(p: string): string {
		const slash = p.lastIndexOf('/');
		return slash === -1 ? '' : p.slice(0, slash);
	}
</script>

<!-- svelte-ignore a11y_no_noninteractive_tabindex -->
<!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
<div class="commit-view" bind:this={scroller} tabindex="0" role="region" aria-label="Commit diff">
	{#if diff === null && loadError.length > 0}
		<div class="empty">{loadError}</div>
	{:else if diff === null}
		<div class="empty">Loading commit…</div>
	{:else if diff.entries.length === 0}
		<div class="empty">No files changed in this commit.</div>
	{:else}
		<div class="banner">
			<span class="title">{diff.subject}</span>
			<span class="sha">{sha?.slice(0, 7) ?? ''}</span>
			<span class="counts">{diff.entries.length} file{diff.entries.length === 1 ? '' : 's'}</span>
		</div>
		<div class="stack">
			{#each diff.entries as entry, i (`${entry.path}|${diff.commitSha}`)}
				<CommitSection
					path={entry.path}
					status={entry.status}
					commitSha={diff.commitSha}
					parentSha={diff.parentSha}
					eager={i < 2}
					{registerSection}
					{side}
				/>
			{/each}
		</div>
	{/if}
</div>

<style>
	.commit-view {
		--m-review-banner-h: 35px;
		flex: 1;
		min-width: 0;
		min-height: 0;
		display: flex;
		flex-direction: column;
		overflow-y: auto;
		background: var(--m-bg);
		color: var(--m-fg);
		padding: 0 12px 12px;
		gap: 12px;
		outline: none;
	}
	.banner {
		display: flex;
		align-items: baseline;
		gap: 10px;
		margin: 0 -12px;
		padding: 10px 16px 8px;
		color: var(--m-fg-muted);
		font-size: 12px;
		position: sticky;
		top: 0;
		z-index: 3;
		background: var(--m-bg);
		border-bottom: 1px solid var(--m-border);
	}
	.title {
		color: var(--m-fg);
		font-weight: 600;
		font-size: 13px;
		flex: 1;
		min-width: 0;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.sha {
		font-family: var(--m-font-mono, monospace);
		color: var(--m-fg-subtle);
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
