<script lang="ts">
	// Framed prompt — "open a terminal".

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
	<path d="m7 11 2-2-2-2" />
	<path d="M11 13h4" />
	<rect width="18" height="18" x="3" y="3" rx="2" ry="2" />
</svg>
