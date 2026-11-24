<script lang="ts">
	// What the active OpenRouter key has left under its own spend cap,
	// next to the context ring, and on click its spend for the day,
	// week and month. The key, not the account: a team key's cap is
	// the budget that matters, whatever the account holds. A key with
	// no cap shows today's spend instead. Refreshed like the ChatGPT
	// meter: on provider change, after each turn, every 5 minutes and
	// on open.
	import { coder } from '../coder.svelte';
	import { ipc } from '../ipc';
	import type { OpenRouterCredits } from '../protocol';
	import UsageGauge from './UsageGauge.svelte';

	const REFRESH_MS = 5 * 60_000;

	const providerId = $derived.by(() => {
		const settings = coder.modelSettings;
		const active = settings?.providers.find((p) => p.id === settings.active_provider);
		return active?.kind === 'open_router' && active.has_api_key ? active.id : null;
	});

	let credits = $state<OpenRouterCredits | null>(null);

	async function refresh(id: string): Promise<void> {
		try {
			const next = await ipc.coder.openrouterCredits(id);
			if (providerId === id) {
				credits = next;
			}
		} catch {
			// Keep the last reading rather than flashing an error.
		}
	}

	$effect(() => {
		const id = providerId;
		credits = null;
		if (id === null) {
			return;
		}
		void refresh(id);
		const timer = setInterval(() => void refresh(id), REFRESH_MS);
		return () => clearInterval(timer);
	});

	let wasBusy = false;
	$effect(() => {
		const busy = coder.busy;
		if (wasBusy && !busy && providerId !== null) {
			void refresh(providerId);
		}
		wasBusy = busy;
	});

	/** Dollars, to the cent; `short` drops the cents from $100 up,
	 *  for the pill. */
	function usd(n: number, short = false): string {
		const abs = Math.abs(n);
		return `${n < 0 ? '-' : ''}$${abs.toFixed(short && abs >= 100 ? 0 : 2)}`;
	}

	const cap = $derived(credits?.key_limit ?? null);
	const remaining = $derived(cap === null ? null : Math.max(0, credits?.key_limit_remaining ?? 0));
	const left = $derived(cap === null || remaining === null || cap <= 0 ? 100 : (remaining / cap) * 100);
	const tone = $derived<'muted' | 'warning' | 'danger'>(
		cap === null ? 'muted' : left <= 10 ? 'danger' : left <= 25 ? 'warning' : 'muted',
	);
	const resetWord = $derived(credits?.key_limit_reset ? `, resets ${credits.key_limit_reset}` : '');
	const summary = $derived.by(() => {
		if (credits === null) {
			return '';
		}
		if (cap === null || remaining === null) {
			return `OpenRouter key: ${usd(credits.key_usage_daily)} spent today, no spend cap`;
		}
		return `OpenRouter key: ${usd(remaining)} left of ${usd(cap)}${resetWord}`;
	});
</script>

{#if providerId !== null && credits !== null}
	{@const key = credits}
	<UsageGauge
		{left}
		value={cap === null ? `${usd(key.key_usage_daily, true)} today` : usd(remaining ?? 0, true)}
		{tone}
		{summary}
		label="OpenRouter key spend"
		onOpen={() => void refresh(providerId)}
	>
		<div class="usage-head">
			OpenRouter key
			{#if cap !== null && (remaining ?? 0) <= 0}<span class="usage-reached">cap reached</span>{/if}
		</div>
		{#if cap !== null}
			<div class="usage-window">
				<div class="usage-row">
					<span class="usage-label">Cap {usd(cap)}{resetWord}</span>
					<span class="usage-left">{usd(remaining ?? 0)} left</span>
				</div>
				<div class="usage-bar">
					<div
						class="usage-fill"
						class:warning={left <= 25 && left > 10}
						class:danger={left <= 10}
						style:width="{left}%"
					></div>
				</div>
			</div>
		{:else}
			<div class="usage-note">No spend cap on this key: it draws on the account's credits.</div>
		{/if}
		<div class="usage-window">
			<div class="usage-row"><span class="usage-label">Today</span><span>{usd(key.key_usage_daily)}</span></div>
			<div class="usage-row"><span class="usage-label">This week</span><span>{usd(key.key_usage_weekly)}</span></div>
			<div class="usage-row"><span class="usage-label">This month</span><span>{usd(key.key_usage_monthly)}</span></div>
			<div class="usage-row"><span class="usage-label">All time</span><span>{usd(key.key_usage)}</span></div>
		</div>
		<div class="usage-note">Day, week and month are UTC.</div>
	</UsageGauge>
{/if}
