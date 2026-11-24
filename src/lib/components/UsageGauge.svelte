<script lang="ts">
	// The pill next to the context ring that says how much of a
	// provider's allowance is left (a ChatGPT plan's window, an
	// OpenRouter key's cap), and the card it opens with the details.
	import type { Snippet } from 'svelte';

	type Props = {
		/** Share left, 0-100: where the needle points. */
		left: number;
		/** What the pill reads (`72%`, `$4.20`). */
		value: string;
		tone: 'muted' | 'warning' | 'danger';
		/** One line for the tooltip and screen readers. */
		summary: string;
		/** The card's name for screen readers. */
		label: string;
		onOpen: () => void;
		children: Snippet;
	};
	let { left, value, tone, summary, label, onOpen, children }: Props = $props();

	let open = $state(false);
	let pill: HTMLButtonElement | undefined = $state();
	let popover: HTMLDivElement | undefined = $state();
	let popoverPos = $state({ top: 0, right: 0 });

	const angle = $derived(Math.PI * (1 - Math.min(100, Math.max(0, left)) / 100));

	function toggle(): void {
		open = !open;
		if (!open || pill === undefined) {
			return;
		}
		const rect = pill.getBoundingClientRect();
		popoverPos = { top: rect.bottom + 6, right: Math.max(8, window.innerWidth - rect.right) };
		onOpen();
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

<button
	bind:this={pill}
	type="button"
	class="usage tone-{tone}"
	title={summary}
	aria-label={summary}
	aria-expanded={open}
	onclick={toggle}
>
	<svg viewBox="0 0 16 16" width="13" height="13" aria-hidden="true">
		<path d="M2.5 11a5.5 5.5 0 1 1 11 0" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" />
		<path
			d="M8 11l{(3.2 * Math.cos(angle)).toFixed(2)} {(-3.2 * Math.sin(angle)).toFixed(2)}"
			fill="none"
			stroke="currentColor"
			stroke-width="1.6"
			stroke-linecap="round"
		/>
	</svg>
	<span class="usage-value">{value}</span>
</button>
{#if open}
	<div
		bind:this={popover}
		class="usage-popover"
		role="dialog"
		aria-label={label}
		style:top="{popoverPos.top}px"
		style:right="{popoverPos.right}px"
	>
		{@render children()}
	</div>
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
	/* The card's parts, shared by every meter. */
	.usage-popover :global(.usage-head) {
		display: flex;
		align-items: center;
		gap: 6px;
		font-weight: 600;
	}
	.usage-popover :global(.usage-reached) {
		margin-left: auto;
		font-weight: 500;
		font-size: 11px;
		color: var(--m-danger);
	}
	.usage-popover :global(.usage-window) {
		display: flex;
		flex-direction: column;
		gap: 4px;
	}
	.usage-popover :global(.usage-row) {
		display: flex;
		justify-content: space-between;
	}
	.usage-popover :global(.usage-label) {
		color: var(--m-fg-muted);
	}
	.usage-popover :global(.usage-left) {
		font-variant-numeric: tabular-nums;
	}
	.usage-popover :global(.usage-bar) {
		height: 4px;
		border-radius: 2px;
		background: var(--m-bg-overlay);
		overflow: hidden;
	}
	.usage-popover :global(.usage-fill) {
		height: 100%;
		background: var(--m-accent);
	}
	.usage-popover :global(.usage-fill.warning) {
		background: var(--m-warning);
	}
	.usage-popover :global(.usage-fill.danger) {
		background: var(--m-danger);
	}
	.usage-popover :global(.usage-note) {
		font-size: 11px;
		color: var(--m-fg-subtle);
	}
</style>
