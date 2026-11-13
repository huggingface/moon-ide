<script lang="ts">
	// Play glyph (Lucide `play`) for the composer's "Go".

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
	style="flex-shrink: 0"
>
	<polygon points="6 3 20 12 6 21 6 3" />
</svg>
