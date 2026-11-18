<script lang="ts">
	// Lines added and removed, as `+12 −3`. Both `null` is a binary
	// file.
	type Props = { additions: number | null; deletions: number | null };
	let { additions, deletions }: Props = $props();
</script>

{#if additions === null || deletions === null}
	<span class="stat binary">binary</span>
{:else}
	<span class="stat" title={`${additions} added, ${deletions} removed`}>
		<span class="add">+{additions}</span>
		<span class="del">−{deletions}</span>
	</span>
{/if}

<style>
	.stat {
		display: inline-flex;
		gap: var(--m-space-1);
		flex-shrink: 0;
		font-size: var(--m-text-xs);
		font-variant-numeric: tabular-nums;
		white-space: nowrap;
	}
	.binary {
		color: var(--m-fg-subtle);
	}
	.add {
		color: var(--m-green);
	}
	.del {
		color: var(--m-red);
	}
</style>
