<script lang="ts">
	// Curved "undo" arrow — used by the per-file revert button on
	// the editor toolbar and by the SCM panel's "revert all
	// changes" button. Both call into `workspace.discardPaths`,
	// just with different scopes; sharing the icon makes the
	// gesture read as the same operation at different scales.

	type Props = { size?: number };
	let { size = 14 }: Props = $props();
</script>

<svg
	xmlns="http://www.w3.org/2000/svg"
	width={size}
	height={size}
	viewBox="0 0 24 24"
	fill="none"
	stroke="currentColor"
	stroke-width="2"
	stroke-linecap="round"
	stroke-linejoin="round"
	aria-hidden="true"
	focusable="false"
>
	<path d="M3 7v6h6" />
	<path d="M21 17a9 9 0 0 0-15-6.7L3 13" />
</svg>
