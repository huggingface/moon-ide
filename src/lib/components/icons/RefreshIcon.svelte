<script lang="ts">
	// Curved arrow + tick — generic "refresh / re-fetch" glyph.

	type Props = { size?: number };
	let { size = 14 }: Props = $props();
</script>

<svg
	xmlns="http://www.w3.org/2000/svg"
	width={size}
	height={size}
	viewBox="0 0 16 16"
	fill="none"
	stroke="currentColor"
	stroke-width="1.5"
	stroke-linecap="round"
	stroke-linejoin="round"
	aria-hidden="true"
	focusable="false"
>
	<path d="M14 8a6 6 0 1 1-1.76-4.24" />
	<path d="M14 2v4h-4" />
</svg>
