<script lang="ts">
	// Single source of truth for the IDE's "container" glyph —
	// Lucide-style 3D-prism. Used wherever a container concept
	// appears in the UI:
	//
	// - Project bar: a folder shipping a `docker-compose.yml`
	//   shows this glyph tinted by the compose project's state.
	// - Terminal tabs / coder bash-target chip: routed through
	//   `TerminalTargetIcon`, which picks between this and
	//   `HostIcon` to mean "running in the container" vs "running
	//   on the host".
	//
	// `currentColor` flows in so the caller's parent CSS
	// (`.indicator.state-running`, `.target-chip.container`, …)
	// drives tint without each call site forking the SVG.

	type Props = { size?: number };
	let { size = 14 }: Props = $props();
</script>

<svg
	xmlns="http://www.w3.org/2000/svg"
	width={size}
	height={size}
	viewBox="0 0 24 24"
	fill="none"
	stroke="currentColor"
	stroke-width="2"
	stroke-linecap="round"
	stroke-linejoin="round"
	aria-hidden="true"
	focusable="false"
>
	<path
		d="M22 7.7c0-.6-.4-1.2-.8-1.5l-6.3-3.9a1.72 1.72 0 0 0-1.7 0l-10.3 6c-.5.2-.9.8-.9 1.4v6.6c0 .5.4 1.2.8 1.5l6.3 3.9a1.72 1.72 0 0 0 1.7 0l10.3-6c.5-.3.9-1 .9-1.5Z"
	/>
	<path d="M10 21.9V14L2.1 9.1" />
	<path d="m10 14 11.9-6.9" />
	<path d="M14 19.8v-8.1" />
	<path d="M18 17.5V9.4" />
</svg>
