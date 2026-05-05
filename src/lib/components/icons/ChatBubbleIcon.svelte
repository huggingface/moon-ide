<script lang="ts">
	// Speech-bubble glyph for the "Switch to chat" affordance on
	// the coder header. Generic shape rather than a Slack hash so
	// the panel can host non-Slack chats later without the icon
	// reading as backend-specific.

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
	stroke-width="1.4"
	stroke-linecap="round"
	stroke-linejoin="round"
	aria-hidden="true"
	focusable="false"
>
	<path
		d="M2.5 4a1.5 1.5 0 0 1 1.5-1.5h8A1.5 1.5 0 0 1 13.5 4v5a1.5 1.5 0 0 1-1.5 1.5H6.5L3.5 13v-2.5H4A1.5 1.5 0 0 1 2.5 9z"
	/>
</svg>
