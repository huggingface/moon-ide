<script lang="ts">
	// Two stacked side-by-side rectangles with a vertical split —
	// glyph for the "Review changes" affordance in the SCM panel
	// header. Reads as "aggregated diff view" without needing a
	// label.

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
	<rect x="2" y="3" width="12" height="10" rx="1.5" />
	<line x1="8" y1="3" x2="8" y2="13" />
	<line x1="3.75" y1="6" x2="6.25" y2="6" />
	<line x1="3.75" y1="9" x2="6.25" y2="9" />
	<line x1="9.75" y1="6" x2="12.25" y2="6" />
	<line x1="9.75" y1="9" x2="12.25" y2="9" />
</svg>
