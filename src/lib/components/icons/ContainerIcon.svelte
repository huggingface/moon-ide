<script lang="ts">
	// Stylised shipping-container outline used in the project bar
	// to mark folders that have a `docker-compose.yml`. Replaces an
	// earlier plain dot — the icon makes the row scannable when a
	// workspace has many folders, only some of which run services.
	// Color comes from `currentColor` so the parent button can tint
	// it per compose state (running / failed / paused / stopped /
	// absent) without forking the SVG.

	type Props = { size?: number };
	let { size = 12 }: Props = $props();
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
	<rect x="2" y="4" width="12" height="8" rx="1" />
	<path d="M5 4v8M8 4v8M11 4v8" />
</svg>
