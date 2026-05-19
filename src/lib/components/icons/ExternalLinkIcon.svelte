<script lang="ts">
	// Universal "open in browser / open external" glyph — box with
	// an arrow exiting the top-right corner. Used by the coder
	// session list to launch a trace's Hub URL in the host's
	// default browser (and Alt-click to copy the URL).

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
	<path d="M9 3h4v4" />
	<path d="M13 3 7.5 8.5" />
	<path d="M13 9.5V13H3V3h3.5" />
</svg>
