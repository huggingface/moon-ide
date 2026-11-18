<script lang="ts">
	// Lines added and removed, as `+12 −3`. Both `null` is a binary
	// file. `tests` is what test files add on top, left out of the
	// figures and named in the tooltip; `muted` drops the colours, for
	// a file that isn't counted.
	type Props = {
		additions: number | null;
		deletions: number | null;
		tests?: { additions: number; deletions: number } | undefined;
		muted?: boolean;
	};
	let { additions, deletions, tests, muted = false }: Props = $props();

	const testsCounted = $derived(tests !== undefined && tests.additions + tests.deletions > 0);
	const title = $derived(
		testsCounted && tests !== undefined
			? `${additions} added, ${deletions} removed, plus +${tests.additions} −${tests.deletions} in tests`
			: `${additions} added, ${deletions} removed${muted ? ' (tests, not counted in the total)' : ''}`,
	);
</script>

{#if additions === null || deletions === null}
	<span class="stat binary">binary</span>
{:else}
	<span class="stat" class:muted {title}>
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
	.binary,
	.muted {
		color: var(--m-fg-subtle);
	}
	.stat:not(.muted) .add {
		color: var(--m-success);
	}
	.stat:not(.muted) .del {
		color: var(--m-danger);
	}
</style>
