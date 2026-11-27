<script lang="ts">
	// ChatGPT or Claude plan usage next to the context ring (ADR 0101, 0131): a small
	// gauge with what's left of the tighter window, and on click the
	// 5 h and weekly windows with their reset times. Only for a
	// signed-in ChatGPT provider. Refreshed when the provider changes,
	// after each turn, every 5 minutes, and on open.
	import { coder } from '../coder.svelte';
	import { ipc } from '../ipc';
	import UsageGauge from './UsageGauge.svelte';
	import type { ChatGptUsage, ChatGptUsageWindow } from '../protocol';

	const REFRESH_MS = 5 * 60_000;

	const providerId = $derived.by(() => {
		const settings = coder.modelSettings;
		const active = settings?.providers.find((p) => p.id === settings.active_provider);
		return (active?.kind === 'chat_gpt' || active?.kind === 'claude_code') && active.has_api_key ? active.id : null;
	});
	/** Claude plan figures come from Claude Code's own reports during turns (ADR 0131). */
	const planName = $derived.by(() => {
		const settings = coder.modelSettings;
		const active = settings?.providers.find((p) => p.id === settings.active_provider);
		return active?.kind === 'claude_code' ? 'Claude' : 'ChatGPT';
	});

	let usage = $state<ChatGptUsage | null>(null);
	let now = $state(Date.now());

	async function refresh(id: string): Promise<void> {
		try {
			const next = await ipc.coder.chatgptUsage(id);
			if (providerId === id) {
				usage = next;
				now = Date.now();
			}
		} catch {
			// Private endpoint; keep the last reading rather than
			// flashing an error in the header.
		}
	}

	$effect(() => {
		const id = providerId;
		usage = null;
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

	const windows = $derived(
		[usage?.primary, usage?.secondary].filter((w): w is ChatGptUsageWindow => w !== null && w !== undefined),
	);
	// What's left of the tighter window: the number that decides
	// whether the next turn goes through.
	const left = $derived(windows.length === 0 ? null : Math.min(...windows.map((w) => 100 - w.used_percent)));
	const tone = $derived<'muted' | 'warning' | 'danger'>(
		usage?.limit_reached || (left !== null && left <= 10)
			? 'danger'
			: left !== null && left <= 25
				? 'warning'
				: 'muted',
	);

	function windowLabel(w: ChatGptUsageWindow, fallback: string): string {
		const s = w.window_seconds;
		if (s === null || s === undefined) {
			return fallback;
		}
		if (s >= 6 * 86_400 && s <= 8 * 86_400) {
			return 'Weekly';
		}
		return s >= 86_400 ? `${Math.round(s / 86_400)}d` : `${Math.round(s / 3_600)}h`;
	}

	function resetLabel(w: ChatGptUsageWindow): string {
		if (w.resets_at === null || w.resets_at === undefined) {
			return '';
		}
		const at = new Date(w.resets_at * 1000);
		const mins = Math.max(0, Math.round((at.getTime() - now) / 60_000));
		const time = at.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
		if (mins < 24 * 60) {
			const h = Math.floor(mins / 60);
			const rel = h > 0 ? `${h}h ${mins % 60}m` : `${mins}m`;
			return `resets ${time} (in ${rel})`;
		}
		return `resets ${at.toLocaleDateString([], { weekday: 'short' })} ${time}`;
	}

	const summary = $derived(
		windows
			.map((w, i) => `${windowLabel(w, i === 0 ? '5h' : 'Weekly')}: ${Math.round(100 - w.used_percent)}% left`)
			.join(' · '),
	);

	function onOpen(): void {
		now = Date.now();
		if (providerId !== null) {
			void refresh(providerId);
		}
	}
</script>

{#if providerId !== null && left !== null && usage !== null}
	{@const plan = usage}
	<UsageGauge
		{left}
		value={`${Math.round(left)}%`}
		{tone}
		summary={`${planName} plan: ${summary}`}
		label={`${planName} plan usage`}
		{onOpen}
	>
		<div class="usage-head">
			{planName}{plan.plan_type ? ` ${plan.plan_type}` : ''} plan
			{#if plan.limit_reached}<span class="usage-reached">limit reached</span>{/if}
		</div>
		{#each windows as w, i (i)}
			{@const pct = 100 - w.used_percent}
			<div class="usage-window">
				<div class="usage-row">
					<span class="usage-label">{windowLabel(w, i === 0 ? '5h' : 'Weekly')}</span>
					<span class="usage-left">{Math.round(pct)}% left</span>
				</div>
				<div class="usage-bar">
					<div
						class="usage-fill"
						class:warning={pct <= 25 && pct > 10}
						class:danger={pct <= 10}
						style:width="{pct}%"
					></div>
				</div>
				<div class="usage-note">{resetLabel(w)}</div>
			</div>
		{/each}
		{#if plan.credits_balance}
			<div class="usage-note">Credits: {plan.credits_balance}</div>
		{/if}
		{#if plan.resets_available}
			<div class="usage-note">
				{plan.resets_available} banked reset{plan.resets_available === 1 ? '' : 's'} available (use it from Codex)
			</div>
		{/if}
	</UsageGauge>
{/if}
