<script lang="ts">
	// "new", "deleted", ... beside a file's name. Nothing for a file
	// that is merely modified.
	import { statusColor, statusWord } from '../fileIcon';
	import type { GitFileStatus } from '../protocol';

	type Props = { status: GitFileStatus };
	let { status }: Props = $props();

	const word = $derived(statusWord(status));
</script>

{#if word !== null}
	<span class="word" style:--status={statusColor(status)}>{word}</span>
{/if}

<style>
	.word {
		flex-shrink: 0;
		padding: 0 6px;
		border-radius: var(--m-radius-pill);
		color: var(--status, var(--m-fg-muted));
		background: color-mix(in srgb, var(--status, var(--m-fg-muted)) 14%, transparent);
		font-size: var(--m-text-xs);
		line-height: 16px;
		white-space: nowrap;
	}
</style>
