<script lang="ts">
	// Smartphone outline — the companion phone bridge.

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
	<rect width="14" height="20" x="5" y="2" rx="2" ry="2" />
	<path d="M12 18h.01" />
</svg>
