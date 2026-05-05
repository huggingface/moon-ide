<script lang="ts">
	// Generic "plus" / "new" glyph. Used by every "create something
	// new" affordance — new session, new chat, etc.

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
	<path d="M8 3v10" />
	<path d="M3 8h10" />
</svg>
