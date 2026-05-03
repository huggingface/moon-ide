<script lang="ts">
	// Tiny inline SVG icon for "this terminal runs on the host"
	// vs. "this terminal runs in the workspace container". Uses
	// `currentColor` so the chip's parent CSS (host = neutral fg,
	// container = success accent) flows through. Two lines tall —
	// drop-in replacement for the old text chip in the tab strip
	// and the body header.
	//
	// Glyphs are Lucide-style (monitor + container) so they read
	// at small sizes without antialiasing mush.

	type Props = {
		kind: 'host' | 'container';
		size?: number;
	};

	let { kind, size = 14 }: Props = $props();
</script>

{#if kind === 'host'}
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
{:else}
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
{/if}
