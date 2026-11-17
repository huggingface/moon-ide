<script lang="ts">
	// The one pill: a short status or tag label. Tone picks the colour;
	// the tint and border are derived from it.
	import type { Snippet } from 'svelte';

	type Props = {
		tone?: 'neutral' | 'accent' | 'success' | 'warning' | 'danger' | 'merged';
		title?: string;
		children: Snippet;
	};
	let { tone = 'neutral', title, children }: Props = $props();
</script>

<span class="pill {tone}" {title}>{@render children()}</span>

<style>
	.pill {
		--pill-color: var(--m-fg-muted);
		display: inline-flex;
		align-items: center;
		gap: var(--m-space-1);
		flex-shrink: 0;
		padding: 0 7px;
		height: 18px;
		border-radius: var(--m-radius-pill);
		border: 1px solid color-mix(in srgb, var(--pill-color) 30%, transparent);
		background: color-mix(in srgb, var(--pill-color) 12%, transparent);
		color: var(--pill-color);
		font-size: var(--m-text-xs);
		line-height: 1;
		white-space: nowrap;
	}
	.accent {
		--pill-color: var(--m-accent);
	}
	.success {
		--pill-color: var(--m-success);
	}
	.warning {
		--pill-color: var(--m-warning);
	}
	.danger {
		--pill-color: var(--m-danger);
	}
	.merged {
		--pill-color: var(--m-merged);
	}
</style>
