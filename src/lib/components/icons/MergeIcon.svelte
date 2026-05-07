<script lang="ts">
	// Lucide-style "git merge" glyph: a target spine on the left
	// with a branch arcing in from the upper-right circle down
	// onto the lower-left endpoint dot. Mirrors `BranchIcon`'s
	// vocabulary so a "branch" surface and a "merge" surface
	// read as obviously-related-but-distinct gestures.

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
	<circle cx="18" cy="18" r="3" />
	<circle cx="6" cy="6" r="3" />
	<path d="M6 21V9a9 9 0 0 0 9 9" />
</svg>
