<script lang="ts">
	// Cloud + circular-arrows glyph. Used on the coder panel
	// header to open the HF bucket sync settings modal.

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
	<path d="M4 11.5h7.5a2.5 2.5 0 0 0 .25-4.99 4 4 0 0 0-7.84-.85A2.5 2.5 0 0 0 4 11.5Z" />
	<path d="M6.5 9 5 10.5 6.5 12" />
	<path d="M5 10.5h4a1.5 1.5 0 0 0 0-3" />
</svg>
