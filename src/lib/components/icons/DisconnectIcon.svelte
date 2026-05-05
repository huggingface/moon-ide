<script lang="ts">
	// Box + arrow-out + line glyph — "disconnect / sign out / log
	// off". Used by the chat panel's disconnect affordance.

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
	<path d="M9.5 3h-5A1.5 1.5 0 0 0 3 4.5v7A1.5 1.5 0 0 0 4.5 13h5" />
	<path d="M11 11l3-3-3-3" />
	<path d="M14 8H6.5" />
</svg>
