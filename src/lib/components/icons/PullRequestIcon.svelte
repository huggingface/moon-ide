<script lang="ts">
	// Lucide-style "git pull request" glyph: two endpoint dots,
	// the source branch line, and the merge arrow into the
	// target. Same visual language Cursor / VSCode / GitHub
	// itself use for "open a PR" affordances.

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
	<circle cx="6" cy="6" r="3" />
	<path d="M6 9v12" />
	<circle cx="18" cy="18" r="3" />
	<path d="M13 6h3a2 2 0 0 1 2 2v7" />
	<path d="m11 4-2 2 2 2" />
</svg>
