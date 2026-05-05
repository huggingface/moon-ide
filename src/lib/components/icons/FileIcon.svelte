<script lang="ts">
	// Generic document outline. Used for compact "this is a file
	// reference" chips where the basename next to it carries the
	// real signal — no per-language variation, intentionally.

	type Props = { size?: number };
	let { size = 12 }: Props = $props();
</script>

<svg
	xmlns="http://www.w3.org/2000/svg"
	width={size}
	height={size}
	viewBox="0 0 16 16"
	fill="none"
	stroke="currentColor"
	stroke-width="1.4"
	stroke-linecap="round"
	stroke-linejoin="round"
	aria-hidden="true"
	focusable="false"
>
	<path d="M4 2h5l3 3v9a1 1 0 0 1-1 1H4a1 1 0 0 1-1-1V3a1 1 0 0 1 1-1Z" />
	<path d="M9 2v3h3" />
</svg>
