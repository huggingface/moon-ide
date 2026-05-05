<script lang="ts">
	// `</>` glyph — same visual language VS Code uses for "code
	// editor / coder". Conventional shorthand; reads as "switch
	// to the code surface" without needing a label.

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
	<path d="M5.5 4.5 2 8l3.5 3.5" />
	<path d="M10.5 4.5 14 8l-3.5 3.5" />
</svg>
