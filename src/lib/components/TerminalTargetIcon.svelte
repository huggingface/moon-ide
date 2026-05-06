<script lang="ts">
	// Tiny dispatcher between `HostIcon` (monitor glyph, "this runs
	// on the host machine") and `ContainerIcon` (3D-prism glyph,
	// "this runs in the workspace container"). Used by the terminal
	// tab strip / body header and by the coder panel's bash-target
	// chip; both want one component that flips on a `kind` prop
	// rather than each caller branching between the two icons.
	//
	// The actual SVGs live in their respective atomic components so
	// the project bar's compose indicator can mount `ContainerIcon`
	// directly without inheriting "this is a terminal target"
	// semantics. One drawing per glyph, multiple semantic call sites.
	//
	// Color flows through `currentColor`: the parent chip's CSS
	// (`.target-chip.container { color: var(--m-success) }`,
	// `.target-chip.host { color: var(--m-fg-muted) }`) tints the
	// strokes without re-rendering the SVG.

	import ContainerIcon from './icons/ContainerIcon.svelte';
	import HostIcon from './icons/HostIcon.svelte';

	type Props = {
		kind: 'host' | 'container';
		size?: number;
	};

	let { kind, size = 14 }: Props = $props();
</script>

{#if kind === 'host'}
	<HostIcon {size} />
{:else}
	<ContainerIcon {size} />
{/if}
