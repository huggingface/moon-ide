<script lang="ts">
	import { compactionProgressLabel, type TokenUsageState, type CompactionState } from '../coder.svelte';

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
	// radius so the ring looks correct at every size. We pin
	// `pathLength = 100` on the fill circle so the dasharray
	// math is in plain percent — a `dash` of `42` means "fill 42%
	// of the ring" regardless of `radius`. Without this the
	// dasharray was in user units (≈ 47 at size 18); a 1 %
	// ratio came out as a 0.47-unit dash that the round-cap
	// geometry effectively erased.
	const stroke = $derived(Math.max(2, Math.round(size * 0.18)));
	const radius = $derived((size - stroke) / 2);
	// Clamp the visible arc to at least 6 % of the ring whenever
	// any prompt tokens have been billed — a fresh session at
	// 1-3 % of context window was rendering as a sub-pixel arc
	// that disappeared into the track. Above the clamp the arc
	// is exact; below it we show a "something is filling" nub
	// and the tooltip carries the precise percentage.
	const dash = $derived.by(() => {
		if (!usage || ratio <= 0) {
			return 0;
		}
		const minVisible = 6;
		return Math.max(minVisible, ratio * 100);
	});

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
		// Prompt-caching line. Only render when either side is
		// non-zero so non-Anthropic providers (and Anthropic
		// requests before the first cache write lands) don't get
		// a "0 cached" line cluttering the tooltip.
		if (usage.cacheReadTokens > 0 || usage.cacheCreationTokens > 0) {
			const parts: string[] = [];
			if (usage.cacheReadTokens > 0) {
				const sharePct = usage.prompt > 0 ? Math.round((usage.cacheReadTokens / usage.prompt) * 100) : 0;
				parts.push(`${formatKilo(usage.cacheReadTokens)} read (${sharePct}%, -90%)`);
			}
			if (usage.cacheCreationTokens > 0) {
				parts.push(`${formatKilo(usage.cacheCreationTokens)} written (+25%)`);
			}
			lines.push(`cache: ${parts.join(' · ')}`);
		}
		if (compaction?.phase === 'running') {
			const progress = compactionProgressLabel(compaction);
			lines.push(
				progress ? `Compacting older turns into a summary — ${progress}…` : `Compacting older turns into a summary…`,
			);
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
				pathLength="100"
				stroke-dasharray="{dash} {100 - dash}"
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
	/* Tone palette uses repo theme tokens (`--m-*`) so the ring
	   tracks the active light/dark theme. Each token has a
	   reasonable hex fallback for the no-theme-loaded case (e.g.
	   the splash) — those values were what the old `--text-muted`
	   alias was implicitly resolving to via the fallback. */
	.tone-idle {
		color: var(--m-fg-muted, #9aa3b9);
	}
	.tone-muted {
		color: var(--m-fg-muted, #9aa3b9);
	}
	.tone-warning {
		color: var(--m-warning, #d4a017);
	}
	.tone-danger {
		color: var(--m-danger, #c62828);
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
