<script lang="ts">
	// Lucide-style "hub" glyph for coordinator (orchestrator)
	// sessions (ADR 0030): a central node linked to three
	// surrounding worker nodes — reads as "delegates to peers",
	// distinct from SparklesIcon's "AI suggestion". Same stroke
	// vocabulary as BranchIcon / MergeIcon so it sits naturally
	// beside them in the session-bar.

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
	<circle cx="12" cy="5" r="2" />
	<circle cx="5" cy="19" r="2" />
	<circle cx="19" cy="19" r="2" />
	<circle cx="12" cy="12" r="2.2" />
	<line x1="12" y1="7" x2="12" y2="9.8" />
	<line x1="10" y1="13.6" x2="6.6" y2="17" />
	<line x1="14" y1="13.6" x2="17.4" y2="17" />
</svg>
