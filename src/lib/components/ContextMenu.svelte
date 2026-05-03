<script lang="ts">
	// Minimal popover menu consumed by Pierre's `contextMenu.render`
	// callback. Pierre hands us an `anchorRect` (either the row or
	// the right-click point) and a `close` function; we handle
	// positioning, click-outside, Escape, and basic arrow-key
	// navigation. No icon lane, no separators, no submenus — add
	// those when a second menu surface actually needs them.

	import { tick } from 'svelte';
	import type { ContextMenuItem } from './contextMenu';

	type Props = {
		items: readonly ContextMenuItem[];
		anchorRect: { left: number; top: number; width: number; height: number };
		onClose: () => void;
	};

	let { items, anchorRect, onClose }: Props = $props();

	let rootEl: HTMLDivElement | null = $state(null);
	let focusedIndex = $state(-1);

	// Pierre already closes the menu on a lot of events, but we own
	// the DOM so we mirror the dismiss-on-outside-click behaviour to
	// cover taps that miss Pierre's own handling (scroll, programmatic
	// focus, etc.). `pointerdown` wins over `click` here because a
	// click on a menu item shouldn't hit the window-pointerdown path
	// first and close us before the item's own `onclick` runs.
	function onWindowPointerDown(event: PointerEvent) {
		const target = event.target as Node | null;
		if (target && rootEl?.contains(target)) {
			return;
		}
		onClose();
	}

	function onWindowKey(event: KeyboardEvent) {
		if (event.key === 'Escape') {
			event.preventDefault();
			onClose();
			return;
		}
		if (event.key === 'ArrowDown') {
			event.preventDefault();
			focusNext(1);
			return;
		}
		if (event.key === 'ArrowUp') {
			event.preventDefault();
			focusNext(-1);
			return;
		}
		if (event.key === 'Enter' && focusedIndex >= 0) {
			event.preventDefault();
			const item = enabledItems[focusedIndex];
			if (item) {
				pick(item);
			}
		}
	}

	const enabledItems = $derived(items.filter((i) => !i.disabled));

	function focusNext(direction: 1 | -1) {
		if (enabledItems.length === 0) {
			return;
		}
		const len = enabledItems.length;
		const next = focusedIndex < 0 ? (direction === 1 ? 0 : len - 1) : (focusedIndex + direction + len) % len;
		focusedIndex = next;
		void tick().then(() => {
			const btn = rootEl?.querySelector<HTMLButtonElement>(`[data-menu-index="${next}"]`);
			btn?.focus();
		});
	}

	function pick(item: ContextMenuItem) {
		if (item.disabled) {
			return;
		}
		// Close before firing the action so a slow handler doesn't
		// leave a stale popover behind while its IPC resolves.
		onClose();
		item.onSelect();
	}

	// Position: fixed against the viewport. We prefer to anchor below
	// the row, but flip above if we'd run out of room. Horizontal
	// alignment mirrors the anchor's left edge, clamped to keep the
	// popover on-screen. Width is content-driven (`min-content`
	// clamped by `max-width`) so a short "Discard changes" doesn't
	// look absurd next to a long path.
	const MARGIN = 8;
	const ESTIMATED_WIDTH = 220;
	const position = $derived.by(() => {
		const estimatedHeight = 40 + enabledItems.length * 28;
		const viewportW = typeof window === 'undefined' ? 1024 : window.innerWidth;
		const viewportH = typeof window === 'undefined' ? 768 : window.innerHeight;
		const belowOk = anchorRect.top + anchorRect.height + estimatedHeight + MARGIN <= viewportH;
		const top = belowOk
			? anchorRect.top + anchorRect.height + 4
			: Math.max(MARGIN, anchorRect.top - estimatedHeight - 4);
		const left = Math.min(Math.max(MARGIN, anchorRect.left), viewportW - ESTIMATED_WIDTH - MARGIN);
		return { top, left };
	});
</script>

<svelte:window onpointerdown={onWindowPointerDown} onkeydown={onWindowKey} />

<div
	class="context-menu"
	role="menu"
	bind:this={rootEl}
	style="top: {position.top}px; left: {position.left}px;"
	data-file-tree-context-menu-root="true"
>
	{#each items as item, idx (item.id)}
		{@const enabledIdx = enabledItems.indexOf(item)}
		<button
			type="button"
			class="item"
			class:danger={item.kind === 'danger'}
			role="menuitem"
			disabled={item.disabled}
			title={item.title ?? ''}
			data-menu-index={enabledIdx}
			onclick={() => pick(item)}
		>
			<span class="label">{item.label}</span>
		</button>
		{#if idx < items.length - 1 && item.kind !== items[idx + 1]?.kind}
			<div class="divider" aria-hidden="true"></div>
		{/if}
	{/each}
</div>

<style>
	.context-menu {
		position: fixed;
		min-width: 180px;
		max-width: 320px;
		background: var(--m-bg-2);
		border: 1px solid var(--m-border-strong);
		border-radius: 6px;
		padding: 4px;
		box-shadow: 0 6px 24px rgba(0, 0, 0, 0.5);
		z-index: 9999;
		display: flex;
		flex-direction: column;
		gap: 1px;
	}
	.item {
		font: inherit;
		display: flex;
		align-items: center;
		gap: 8px;
		padding: 6px 10px;
		background: transparent;
		color: var(--m-fg);
		border: 1px solid transparent;
		border-radius: 4px;
		cursor: pointer;
		text-align: left;
		font-size: 12px;
		white-space: nowrap;
	}
	.item:hover:not(:disabled),
	.item:focus-visible:not(:disabled) {
		background: var(--m-bg-overlay);
		outline: none;
	}
	.item:disabled {
		color: var(--m-fg-muted);
		cursor: default;
	}
	.item.danger {
		color: var(--m-danger);
	}
	.label {
		flex: 1;
	}
	.divider {
		height: 1px;
		margin: 4px 2px;
		background: var(--m-border);
	}
</style>
