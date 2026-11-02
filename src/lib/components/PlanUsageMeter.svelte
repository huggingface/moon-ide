<script lang="ts">
	// ChatGPT plan usage next to the context ring (ADR 0101): a small
	// gauge with what's left of the tighter window, and on click the
	// 5 h and weekly windows with their reset times. Only for a
	// signed-in ChatGPT provider. Refreshed when the provider changes,
	// after each turn, every 5 minutes, and on open.
	import { coder } from '../coder.svelte';
	import { ipc } from '../ipc';
	import type { ChatGptUsage, ChatGptUsageWindow } from '../protocol';

	const REFRESH_MS = 5 * 60_000;

	const providerId = $derived.by(() => {
		const settings = coder.modelSettings;
		const active = settings?.providers.find((p) => p.id === settings.active_provider);
		return active?.kind === 'chat_gpt' && active.has_api_key ? active.id : null;
	});

	let usage = $state<ChatGptUsage | null>(null);
	let open = $state(false);
	let pill: HTMLButtonElement | undefined = $state();
	let popover: HTMLDivElement | undefined = $state();
	let popoverPos = $state({ top: 0, right: 0 });
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

	function toggle(): void {
		open = !open;
		if (!open || pill === undefined) {
			return;
		}
		const rect = pill.getBoundingClientRect();
		popoverPos = { top: rect.bottom + 6, right: Math.max(8, window.innerWidth - rect.right) };
		now = Date.now();
		if (providerId !== null) {
			void refresh(providerId);
		}
	}

	$effect(() => {
		if (!open) {
			return;
		}
		const onPointerDown = (event: PointerEvent) => {
			const target = event.target;
			if (target instanceof Node && (pill?.contains(target) || popover?.contains(target))) {
				return;
			}
			open = false;
		};
		const onKey = (event: KeyboardEvent) => {
			if (event.key === 'Escape') {
				open = false;
			}
		};
		window.addEventListener('pointerdown', onPointerDown);
		window.addEventListener('keydown', onKey);
		return () => {
			window.removeEventListener('pointerdown', onPointerDown);
			window.removeEventListener('keydown', onKey);
		};
	});
</script>

{#if providerId !== null && left !== null}
	<button
		bind:this={pill}
		type="button"
		class="usage tone-{tone}"
		title={`ChatGPT plan: ${summary}`}
		aria-label={`ChatGPT plan usage: ${summary}`}
		aria-expanded={open}
		onclick={toggle}
	>
		<svg viewBox="0 0 16 16" width="13" height="13" aria-hidden="true">
			<path
				d="M2.5 11a5.5 5.5 0 1 1 11 0"
				fill="none"
				stroke="currentColor"
				stroke-width="1.6"
				stroke-linecap="round"
			/>
			<path
				d="M8 11l{(3.2 * Math.cos(Math.PI * (1 - left / 100))).toFixed(2)} {(
					-3.2 * Math.sin(Math.PI * (1 - left / 100))
				).toFixed(2)}"
				fill="none"
				stroke="currentColor"
				stroke-width="1.6"
				stroke-linecap="round"
			/>
		</svg>
		<span class="usage-value">{Math.round(left)}%</span>
	</button>
	{#if open && usage !== null}
		<div
			bind:this={popover}
			class="usage-popover"
			role="dialog"
			aria-label="ChatGPT plan usage"
			style:top="{popoverPos.top}px"
			style:right="{popoverPos.right}px"
		>
			<div class="usage-head">
				ChatGPT{usage.plan_type ? ` ${usage.plan_type}` : ''} plan
				{#if usage.limit_reached}<span class="usage-reached">limit reached</span>{/if}
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
					<div class="usage-reset">{resetLabel(w)}</div>
				</div>
			{/each}
			{#if usage.credits_balance}
				<div class="usage-credits">Credits: {usage.credits_balance}</div>
			{/if}
			{#if usage.resets_available}
				<div class="usage-credits">
					{usage.resets_available} banked reset{usage.resets_available === 1 ? '' : 's'} available (use it from Codex)
				</div>
			{/if}
		</div>
	{/if}
{/if}

<style>
	.usage {
		display: inline-flex;
		align-items: center;
		gap: 3px;
		padding: 1px 4px;
		border: 1px solid transparent;
		border-radius: 4px;
		background: none;
		font: inherit;
		font-size: 11px;
		font-variant-numeric: tabular-nums;
		cursor: pointer;
		line-height: 1;
	}
	.usage:hover,
	.usage[aria-expanded='true'] {
		border-color: var(--m-border);
	}
	.tone-muted {
		color: var(--m-fg-muted);
	}
	.tone-warning {
		color: var(--m-warning);
	}
	.tone-danger {
		color: var(--m-danger);
	}
	.usage-popover {
		position: fixed;
		z-index: 1000;
		width: 240px;
		padding: 10px 12px;
		display: flex;
		flex-direction: column;
		gap: 10px;
		background: var(--m-bg-2);
		border: 1px solid var(--m-border-strong);
		border-radius: 6px;
		box-shadow: 0 6px 24px rgba(0, 0, 0, 0.5);
		font-size: 12px;
		color: var(--m-fg);
	}
	.usage-head {
		display: flex;
		align-items: center;
		gap: 6px;
		font-weight: 600;
	}
	.usage-reached {
		margin-left: auto;
		font-weight: 500;
		font-size: 11px;
		color: var(--m-danger);
	}
	.usage-window {
		display: flex;
		flex-direction: column;
		gap: 4px;
	}
	.usage-row {
		display: flex;
		justify-content: space-between;
	}
	.usage-label {
		color: var(--m-fg-muted);
	}
	.usage-left {
		font-variant-numeric: tabular-nums;
	}
	.usage-bar {
		height: 4px;
		border-radius: 2px;
		background: var(--m-bg-overlay);
		overflow: hidden;
	}
	.usage-fill {
		height: 100%;
		background: var(--m-accent);
	}
	.usage-fill.warning {
		background: var(--m-warning);
	}
	.usage-fill.danger {
		background: var(--m-danger);
	}
	.usage-reset,
	.usage-credits {
		font-size: 11px;
		color: var(--m-fg-subtle);
	}
</style>
