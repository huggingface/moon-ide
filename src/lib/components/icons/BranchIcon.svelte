<script lang="ts">
	// Lucide-style "git branch" glyph: source spine, branch fork
	// peeling off to a small endpoint dot. Matches the visual
	// vocabulary GitHub / VSCode / Cursor use for "branch" surfaces.

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
	<line x1="6" x2="6" y1="3" y2="15" />
	<circle cx="18" cy="6" r="3" />
	<circle cx="6" cy="18" r="3" />
	<path d="M18 9a9 9 0 0 1-9 9" />
</svg>
