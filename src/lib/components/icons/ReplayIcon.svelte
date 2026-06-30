<script lang="ts">
	// Circular arrow with a play triangle — the coder transcript's
	// per-user-row "replay from here" action: truncate to just
	// before the message and re-send the original prompt verbatim
	// (a re-run of that turn, no composer round-trip). Reads as
	// distinct from the pencil (edit & resend) and the undo arrow
	// (revert / discard).

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
	<path d="M13.5 8a5.5 5.5 0 1 1-1.6-3.9" />
	<path d="M13.5 2.5v3h-3" />
	<path d="M6.5 6.8 10 8.2 6.5 9.6Z" fill="currentColor" stroke="none" />
</svg>
