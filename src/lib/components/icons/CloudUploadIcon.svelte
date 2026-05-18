<script lang="ts">
	// Cloud + up-arrow glyph. Used on the coder session list to
	// indicate "push this trace to the workspace's HF bucket".

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
	<path d="M4 12.5h7.5a2.5 2.5 0 0 0 .25-4.99 4 4 0 0 0-7.84-.85A2.5 2.5 0 0 0 4 12.5Z" />
	<path d="M8 6.5v5" />
	<path d="m6 8.5 2-2 2 2" />
</svg>
