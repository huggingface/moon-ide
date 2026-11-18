<script lang="ts">
	// A pull request's or an issue's `#<number> <title>`, with the
	// number set back so the title is what reads first. Any other name
	// is shown as it is.
	import { splitTabName } from '../util/prPath';

	type Props = { name: string };
	let { name }: Props = $props();

	const parts = $derived(splitTabName(name));
</script>

{#if parts === null}{name}{:else}<span class="ref">{parts.ref}</span>{parts.title === '' ? '' : ` ${parts.title}`}{/if}

<style>
	.ref {
		opacity: 0.55;
		font-weight: 400;
		font-variant-numeric: tabular-nums;
	}
</style>
