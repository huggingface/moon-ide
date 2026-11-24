<script lang="ts">
	import { onMount, onDestroy, tick } from 'svelte';
	import { confirm } from '@tauri-apps/plugin-dialog';
	import { workspace, type SplitSide } from '../state.svelte';
	import { ipc } from '../ipc';
	import { shaFromCommitPath } from '../util/commitPath';
	import { formatError, type CommitDiff } from '../protocol';
	import CommitSection from './CommitSection.svelte';
	import FileRail from './FileRail.svelte';

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

	let reverting = $state(false);

	async function revert() {
		const current = diff;
		if (current === null || reverting) {
			return;
		}
		const ok = await confirm(
			`Create a commit that reverts ${current.commitSha.slice(0, 7)} "${current.subject}"? It stays local until you push.`,
			{ title: 'Revert commit', kind: 'warning' },
		);
		if (!ok) {
			return;
		}
		reverting = true;
		try {
			await workspace.revertCommit(current.commitSha);
		} finally {
			reverting = false;
		}
	}

	function registerSection(path: string, el: HTMLElement | null) {
		if (el === null) {
			sectionEls.delete(path);
			return;
		}
		sectionEls.set(path, el);
	}

	// The files beside the diffs, as in a pull request's tab: docked
	// in a pane wide enough for both, else opened over the diffs from
	// the banner's Files button.
	const DOCK_PX = 1000;
	const BANNER_PX = 37;
	let width = $state(0);
	let railWanted = $state<boolean | null>(null);
	const files = $derived(diff?.entries ?? []);
	const docked = $derived(width >= DOCK_PX);
	const railShown = $derived(files.length > 1 && (railWanted ?? docked));

	// The file under the top of the pane.
	let activePath = $state<string | null>(null);
	let spyFrame = -1;
	function spy() {
		cancelAnimationFrame(spyFrame);
		spyFrame = requestAnimationFrame(() => {
			if (scroller === undefined) {
				return;
			}
			const line = scroller.getBoundingClientRect().top + BANNER_PX + 12;
			let found: string | null = files[0]?.path ?? null;
			for (const file of files) {
				const el = sectionEls.get(file.path);
				if (el === undefined || el.getBoundingClientRect().top > line) {
					break;
				}
				found = file.path;
			}
			activePath = found;
		});
	}

	async function jumpTo(path: string) {
		if (!docked) {
			railWanted = null;
		}
		await tick();
		const el = sectionEls.get(path);
		if (el !== undefined && scroller !== undefined) {
			scroller.scrollTo({
				top: scroller.scrollTop + el.getBoundingClientRect().top - scroller.getBoundingClientRect().top - BANNER_PX,
			});
		}
		activePath = path;
	}

	onDestroy(() => cancelAnimationFrame(spyFrame));
</script>

<!-- svelte-ignore a11y_no_noninteractive_tabindex -->
<!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
<div class="commit-layout" bind:clientWidth={width}>
	{#if railShown}
		<FileRail {files} {activePath} floating={!docked} label="Files in this commit" onPick={(path) => void jumpTo(path)}>
			{#snippet head()}
				<span>{files.length} files</span>
			{/snippet}
		</FileRail>
	{/if}
	<div class="commit-view" bind:this={scroller} tabindex="0" role="region" aria-label="Commit diff" onscroll={spy}>
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
				{#if files.length > 1}
					<button
						type="button"
						class="files"
						aria-pressed={railShown}
						title="List of changed files"
						onclick={() => (railWanted = !railShown)}
					>
						Files {files.length}
					</button>
				{:else}
					<span class="counts">1 file</span>
				{/if}
				<button type="button" class="revert" onclick={revert} disabled={reverting} title="git revert this commit">
					{reverting ? 'Reverting…' : 'Revert'}
				</button>
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
</div>

<style>
	.commit-layout {
		--m-review-banner-h: 37px;
		position: relative;
		flex: 1;
		min-width: 0;
		min-height: 0;
		display: flex;
	}
	.commit-view {
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
		align-items: center;
		gap: 10px;
		box-sizing: border-box;
		height: var(--m-review-banner-h);
		flex-shrink: 0;
		margin: 0 -12px;
		padding: 0 16px;
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
	.files {
		align-self: center;
		margin-left: auto;
		padding: 3px 10px;
		background: var(--m-bg);
		border: 1px solid var(--m-border);
		border-radius: 4px;
		color: var(--m-fg);
		font-size: 11px;
		font-weight: 600;
		cursor: pointer;
	}
	.files:hover,
	.files[aria-pressed='true'] {
		border-color: var(--m-fg-muted);
	}
	.revert {
		align-self: center;
		padding: 3px 10px;
		background: var(--m-bg);
		border: 1px solid var(--m-border);
		border-radius: 4px;
		color: var(--m-fg);
		font-size: 11px;
		font-weight: 600;
		cursor: pointer;
	}
	.revert:hover:not(:disabled) {
		border-color: var(--m-fg-muted);
	}
	.revert:disabled {
		cursor: default;
		opacity: 0.6;
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
