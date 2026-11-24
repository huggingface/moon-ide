<script lang="ts">
	// What the active OpenRouter key can still spend, next to the
	// context ring, and on click its spend for the day, week and month.
	// Its own cap when it has one (a shared key's cap is the budget
	// that matters, whatever the account holds), else the account's
	// remaining credits, which is what it draws on. Refreshed like the
	// ChatGPT meter: on provider change, after each turn, every 5
	// minutes and on open.
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

	/** What bounds the key's spending: its cap, or the account. */
	const budget = $derived.by(() => {
		if (credits === null) {
			return null;
		}
		const cap = credits.key_limit;
		if (cap !== null && cap !== undefined) {
			const reset = credits.key_limit_reset ? `, resets ${credits.key_limit_reset}` : '';
			return {
				total: cap,
				left: Math.max(0, credits.key_limit_remaining ?? 0),
				label: `Key cap ${usd(cap)}${reset}`,
				spent: 'cap reached',
			};
		}
		return {
			total: credits.total_credits,
			left: Math.max(0, credits.total_credits - credits.total_usage),
			label: `Account credits ${usd(credits.total_credits)}`,
			spent: 'out of credits',
		};
	});
	const left = $derived(budget === null || budget.total <= 0 ? 0 : (budget.left / budget.total) * 100);
	const tone = $derived<'muted' | 'warning' | 'danger'>(left <= 10 ? 'danger' : left <= 25 ? 'warning' : 'muted');
	const summary = $derived(
		budget === null ? '' : `OpenRouter: ${usd(budget.left)} left (${budget.label.toLowerCase()})`,
	);
</script>

{#if providerId !== null && credits !== null && budget !== null}
	{@const key = credits}
	<UsageGauge
		{left}
		value={usd(budget.left, true)}
		{tone}
		{summary}
		label="OpenRouter key spend"
		onOpen={() => void refresh(providerId)}
	>
		<div class="usage-head">
			OpenRouter key
			{#if budget.left <= 0}<span class="usage-reached">{budget.spent}</span>{/if}
		</div>
		<div class="usage-window">
			<div class="usage-row">
				<span class="usage-label">{budget.label}</span>
				<span class="usage-left">{usd(budget.left)} left</span>
			</div>
			<div class="usage-bar">
				<div
					class="usage-fill"
					class:warning={left <= 25 && left > 10}
					class:danger={left <= 10}
					style:width="{left}%"
				></div>
			</div>
			{#if key.key_limit === null || key.key_limit === undefined}
				<div class="usage-note">No spend cap on this key: it draws on the account's credits.</div>
			{/if}
		</div>
		<div class="usage-window">
			<div class="usage-row"><span class="usage-label">Today</span><span>{usd(key.key_usage_daily)}</span></div>
			<div class="usage-row"><span class="usage-label">This week</span><span>{usd(key.key_usage_weekly)}</span></div>
			<div class="usage-row"><span class="usage-label">This month</span><span>{usd(key.key_usage_monthly)}</span></div>
			<div class="usage-row"><span class="usage-label">All time</span><span>{usd(key.key_usage)}</span></div>
		</div>
		<div class="usage-note">Day, week and month are UTC.</div>
	</UsageGauge>
{/if}
