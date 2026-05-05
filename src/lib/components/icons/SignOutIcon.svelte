<script lang="ts">
	// "Door + arrow out" glyph — used by the coder header's "Sign
	// out" affordance.

	type Props = { size?: number };
	let { size = 14 }: Props = $props();
</script>

<svg
	xmlns="http://www.w3.org/2000/svg"
	width={size}
	height={size}
	viewBox="0 0 16 16"
	aria-hidden="true"
	focusable="false"
>
	<path
		d="M9 1H4a1 1 0 0 0-1 1v12a1 1 0 0 0 1 1h5"
		fill="none"
		stroke="currentColor"
		stroke-width="1.4"
		stroke-linecap="round"
	/>
	<path
		d="M11 5l3 3-3 3M14 8H7"
		fill="none"
		stroke="currentColor"
		stroke-width="1.4"
		stroke-linecap="round"
		stroke-linejoin="round"
	/>
</svg>
