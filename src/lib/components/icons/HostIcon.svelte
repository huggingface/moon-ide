<script lang="ts">
	// Lucide-style monitor glyph used wherever the IDE needs to
	// signal "this runs on the host machine" (as opposed to a
	// container). Counterpart to `ContainerIcon`. `currentColor`
	// flows in so callers can tint per state without forking the
	// SVG.

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
	<rect width="20" height="14" x="2" y="3" rx="2" />
	<line x1="8" x2="16" y1="21" y2="21" />
	<line x1="12" x2="12" y1="17" y2="21" />
</svg>
