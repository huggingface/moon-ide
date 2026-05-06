<script lang="ts">
	import type { TokenUsageState, CompactionState } from '../coder.svelte';

	type Props = {
		usage: TokenUsageState | null;
		compaction: CompactionState | null;
		size?: number;
	};

	const { usage, compaction, size = 18 }: Props = $props();

	// Hardcoded thresholds matching the runner's compaction trigger.
	// Anything ≥ DANGER will compact on the next round-trip; the
	// WARNING band is a heads-up that we're getting close. Mirroring
	// these to the UI lets the user see the compaction coming before
	// the panel suddenly shows a "compacting…" pip.
	const WARNING = 0.6;
	const DANGER = 0.8;

	const ratio = $derived.by(() => {
		if (!usage || usage.contextWindow <= 0) {
			return 0;
		}
		return Math.min(1, usage.prompt / usage.contextWindow);
	});

	const tone = $derived.by<'idle' | 'muted' | 'warning' | 'danger'>(() => {
		if (!usage) {
			return 'idle';
		}
		if (ratio >= DANGER) {
			return 'danger';
		}
		if (ratio >= WARNING) {
			return 'warning';
		}
		return 'muted';
	});

	// SVG ring geometry: the stroke width is a fraction of the
	// radius so the ring looks correct at every size.
	const stroke = $derived(Math.max(2, Math.round(size * 0.18)));
	const radius = $derived((size - stroke) / 2);
	const circumference = $derived(2 * Math.PI * radius);
	const dash = $derived(circumference * ratio);

	function formatKilo(n: number): string {
		if (n < 1000) {
			return `${n}`;
		}
		// 1k-multiples per AGENTS.md house rules. One decimal under
		// 100k for readability (12.4k), no decimals at 100k+ (174k).
		const k = n / 1000;
		if (k >= 100) {
			return `${Math.round(k)}k`;
		}
		return `${k.toFixed(1)}k`;
	}

	const tooltip = $derived.by(() => {
		if (!usage) {
			return 'No turns yet.';
		}
		const pct = Math.round(ratio * 100);
		const prefix = usage.source === 'estimate' ? '≈ ' : '';
		const ctxKilo = formatKilo(usage.contextWindow);
		const promptKilo = formatKilo(usage.prompt);
		const lines = [`${prefix}${promptKilo} / ${ctxKilo} prompt tokens (${pct}% of context window)`];
		lines.push(`${prefix}${formatKilo(usage.completion)} completion · ${formatKilo(usage.total)} total`);
		if (usage.source === 'estimate') {
			lines.push('Provider did not emit usage; figures are bytes/4 estimates.');
		}
		if (compaction?.phase === 'running') {
			lines.push(`Compacting older turns into a summary…`);
		} else if (compaction?.phase === 'done') {
			lines.push(`Last compaction folded ${compaction.messagesCompacted} messages into a summary.`);
		}
		return lines.join('\n');
	});
</script>

<span
	class="ring tone-{tone}"
	class:pulse={compaction?.phase === 'running'}
	title={tooltip}
	aria-label={tooltip}
	style="width: {size}px; height: {size}px;"
>
	<svg
		xmlns="http://www.w3.org/2000/svg"
		viewBox="0 0 {size} {size}"
		width={size}
		height={size}
		role="presentation"
		focusable="false"
	>
		<circle
			cx={size / 2}
			cy={size / 2}
			r={radius}
			fill="none"
			stroke="currentColor"
			stroke-width={stroke}
			class="track"
		/>
		{#if usage}
			<circle
				cx={size / 2}
				cy={size / 2}
				r={radius}
				fill="none"
				stroke="currentColor"
				stroke-width={stroke}
				stroke-linecap="round"
				stroke-dasharray="{dash} {circumference}"
				transform="rotate(-90 {size / 2} {size / 2})"
				class="fill"
			/>
		{/if}
	</svg>
</span>

<style>
	.ring {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		flex-shrink: 0;
		line-height: 0;
	}
	.track {
		opacity: 0.18;
	}
	.tone-idle {
		color: var(--text-muted, #888);
	}
	.tone-muted {
		color: var(--text-muted, #888);
	}
	.tone-warning {
		color: var(--warning, #d4a017);
	}
	.tone-danger {
		color: var(--danger, #c62828);
	}
	.pulse {
		animation: ring-pulse 1.6s ease-in-out infinite;
	}
	@keyframes ring-pulse {
		0%,
		100% {
			opacity: 1;
		}
		50% {
			opacity: 0.45;
		}
	}
</style>
