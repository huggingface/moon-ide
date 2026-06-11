<script lang="ts">
	// Speech bubble with a question mark — the "agent is asking you
	// something and is waiting for an answer" cue. Used on the
	// folder bar and the session-list row when a turn is parked on
	// an `ask_user` prompt, so it reads distinctly from the plain
	// "running" sparkle / pulse.

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
	<path d="M6.7 5.55a1.35 1.35 0 0 1 2.4.85c0 .9-1.1 1.05-1.1 1.85" stroke-width="1.2" />
	<circle cx="8" cy="9.7" r="0.55" fill="currentColor" stroke="none" />
</svg>
