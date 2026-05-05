<script lang="ts">
	// Three-line "all conversations" list glyph — same visual
	// language Slack / Discord / Cursor use for the sessions list
	// header.

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
	<path d="M3 4h10" />
	<path d="M3 8h10" />
	<path d="M3 12h10" />
</svg>
