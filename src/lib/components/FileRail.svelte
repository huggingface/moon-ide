<script lang="ts">
	// The list of a diff's files beside it (a pull request's, a
	// commit's): the file under the top of the pane is highlighted, a
	// click jumps to it. Docked beside the diffs in a wide pane; in a
	// narrow one it opens over them (`floating`), from the banner.
	import { tick, type Snippet } from 'svelte';
	import { statusColor } from '../fileIcon';
	import type { GitFileStatus } from '../protocol';
	import FileIcon from './FileIcon.svelte';

	type Props = {
		files: readonly { path: string; status: GitFileStatus }[];
		activePath: string | null;
		floating: boolean;
		label: string;
		onPick: (path: string) => void;
		/** The row above the files, for what comes before them. */
		onOverview?: (() => void) | undefined;
		/** Paths drawn as done (a pull request's viewed files). */
		done?: (path: string) => boolean;
		head: Snippet;
		/** What ends a file's row (its numbers, a tick). */
		trailing?: Snippet<[string]> | undefined;
	};
	let { files, activePath, floating, label, onPick, onOverview, done = () => false, head, trailing }: Props = $props();

	let list: HTMLDivElement | undefined = $state();
	// Keeps the current file in sight in a list longer than the pane.
	$effect(() => {
		void activePath;
		void tick().then(() => list?.querySelector('.rail-item.active')?.scrollIntoView({ block: 'nearest' }));
	});

	function baseName(path: string): string {
		return path.slice(path.lastIndexOf('/') + 1);
	}

	function dirName(path: string): string {
		const cut = path.lastIndexOf('/');
		return cut === -1 ? '' : path.slice(0, cut);
	}
</script>

<nav class="rail" class:floating aria-label={label}>
	<div class="rail-head">{@render head()}</div>
	<div class="rail-list" bind:this={list}>
		{#if onOverview !== undefined}
			<button type="button" class="rail-item" class:active={activePath === null} onclick={onOverview}>
				<span class="rail-name">Overview</span>
			</button>
		{/if}
		{#each files as file (file.path)}
			{@const isDone = done(file.path)}
			<button
				type="button"
				class="rail-item"
				class:active={activePath === file.path}
				class:viewed={isDone}
				title={`${file.path} (${file.status})`}
				onclick={() => onPick(file.path)}
			>
				<FileIcon path={file.path} size={14} />
				<span
					class="rail-name"
					class:gone={file.status === 'deleted'}
					style:color={isDone ? undefined : statusColor(file.status)}
				>
					{baseName(file.path)}
				</span>
				<span class="rail-dir">{dirName(file.path)}</span>
				{@render trailing?.(file.path)}
			</button>
		{/each}
	</div>
</nav>

<style>
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
		top: var(--m-review-banner-h, 37px);
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
		height: var(--m-review-banner-h, 37px);
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
</style>
