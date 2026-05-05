<script lang="ts">
	// Two-direction arrows — "swap" / "switch between two things"
	// glyph. Used by the chat panel's "switch bot" affordance.

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
	<path d="M3 5h9" />
	<path d="M9 2l3 3-3 3" />
	<path d="M13 11H4" />
	<path d="M7 8l-3 3 3 3" />
</svg>
