<script lang="ts">
	// Trash-can glyph for destructive "delete this entry" actions
	// (delete session, delete file in the future, etc.). Hover
	// styling lives at the call site so it can pick up
	// `--m-danger` only when appropriate.

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
	<path d="M3 4h10" />
	<path d="M5 4V3a1 1 0 0 1 1-1h4a1 1 0 0 1 1 1v1" />
	<path d="M4.5 4l.7 9a1 1 0 0 0 1 1h3.6a1 1 0 0 0 1-1l.7-9" />
</svg>
