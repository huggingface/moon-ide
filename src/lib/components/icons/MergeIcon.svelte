<script lang="ts">
	// "Merge into" glyph: a vertical trunk (the target branch) with a
	// branch line curving in from the upper right and ending in an
	// arrowhead on the trunk. Deliberately no endpoint circles — the
	// Lucide `git-merge` glyph used before collapsed into two dots at
	// the 13px folder-bar size and read as a bullet, not an action.

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
	stroke-width="2.25"
	stroke-linecap="round"
	stroke-linejoin="round"
	aria-hidden="true"
	focusable="false"
	style="flex-shrink: 0"
>
	<line x1="6" y1="3" x2="6" y2="21" />
	<path d="M18 3v3a8 8 0 0 1-8 8H7" />
	<path d="M11 10l-4 4 4 4" />
</svg>
