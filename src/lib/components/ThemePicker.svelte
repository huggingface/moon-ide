<script lang="ts">
	// Three-option popover in the status bar: System / Dark / Light.
	// The trigger shows whichever mode is active (the three-way enum,
	// not the resolved dark/light); clicking flips the popover and
	// picking an item persists the choice through
	// `workspace.setTheme`.
	//
	// We don't expose a cycle button because a three-valued toggle
	// has no obvious order. Same click-outside + Esc dismissal
	// pattern as `TerminalLauncher`.

	import { workspace } from '../state.svelte';
	import type { ThemeMode } from '../protocol';

	type Option = {
		id: ThemeMode;
		label: string;
		icon: string;
	};

	const OPTIONS: Option[] = [
		{ id: 'system', label: 'System', icon: '◐' },
		{ id: 'light', label: 'Light', icon: '☀' },
		{ id: 'dark', label: 'Dark', icon: '☾' },
	];

	let open = $state(false);
	let rootEl: HTMLDivElement | null = $state(null);
	let triggerEl: HTMLButtonElement | undefined = $state();

	export function focus() {
		triggerEl?.focus();
	}

	const activeOption = $derived(OPTIONS.find((o) => o.id === workspace.theme) ?? OPTIONS[0]);
	// Tooltip tells the user both their stored choice and — for
	// `'system'` — what it currently resolves to, so a click can be
	// predicted without opening the popover.
	const triggerTitle = $derived(
		workspace.theme === 'system'
			? `Theme: System (currently ${workspace.effectiveTheme}) — click to change`
			: `Theme: ${workspace.theme} — click to change`,
	);

	function toggle() {
		open = !open;
	}

	function close() {
		open = false;
	}

	function pick(mode: ThemeMode) {
		workspace.setTheme(mode);
		close();
		// Return focus to the trigger so keyboard flow isn't stranded
		// on a popover that just unmounted.
		queueMicrotask(() => triggerEl?.focus());
	}

	function onWindowPointerDown(event: PointerEvent) {
		if (!open) {
			return;
		}
		const target = event.target as Node | null;
		if (target && rootEl?.contains(target)) {
			return;
		}
		close();
	}

	function onWindowKey(event: KeyboardEvent) {
		if (!open) {
			return;
		}
		if (event.key === 'Escape') {
			close();
			queueMicrotask(() => triggerEl?.focus());
		}
	}
</script>

<svelte:window onpointerdown={onWindowPointerDown} onkeydown={onWindowKey} />

<div class="picker" bind:this={rootEl}>
	<button bind:this={triggerEl} type="button" class="trigger" class:open title={triggerTitle} onclick={toggle}>
		<span class="icon" aria-hidden="true">{activeOption?.icon}</span>
		<span class="label">{activeOption?.label.toLowerCase()}</span>
	</button>
	{#if open}
		<div class="menu" role="menu">
			{#each OPTIONS as option (option.id)}
				<button
					type="button"
					class="item"
					class:selected={workspace.theme === option.id}
					role="menuitemradio"
					aria-checked={workspace.theme === option.id}
					onclick={() => pick(option.id)}
				>
					<span class="item-icon" aria-hidden="true">{option.icon}</span>
					<span class="item-title">{option.label}</span>
					{#if option.id === 'system'}
						<span class="item-sub">{workspace.effectiveTheme}</span>
					{/if}
				</button>
			{/each}
		</div>
	{/if}
</div>

<style>
	.picker {
		position: relative;
		display: inline-flex;
	}
	.trigger {
		font: inherit;
		color: var(--m-fg-muted);
		background: transparent;
		border: 1px solid transparent;
		border-radius: 4px;
		padding: 0 6px;
		height: 18px;
		line-height: 18px;
		display: inline-flex;
		align-items: center;
		gap: 5px;
		cursor: pointer;
	}
	.trigger:hover,
	.trigger.open {
		background: var(--m-bg-overlay);
		color: var(--m-fg);
	}
	.icon {
		font-size: 12px;
		line-height: 1;
	}
	.label {
		font-size: 11px;
	}
	.menu {
		position: absolute;
		right: 0;
		bottom: 100%;
		margin-bottom: 6px;
		min-width: 180px;
		background: var(--m-bg-2);
		border: 1px solid var(--m-border-strong);
		border-radius: 6px;
		padding: 4px;
		box-shadow: 0 6px 24px rgba(0, 0, 0, 0.5);
		z-index: 30;
		display: flex;
		flex-direction: column;
		gap: 2px;
	}
	.item {
		font: inherit;
		display: flex;
		align-items: center;
		gap: 8px;
		padding: 6px 8px;
		background: transparent;
		color: var(--m-fg);
		border: 1px solid transparent;
		border-radius: 4px;
		cursor: pointer;
		text-align: left;
		font-size: 12px;
	}
	.item:hover {
		background: var(--m-bg-overlay);
		border-color: var(--m-border);
	}
	.item.selected {
		background: var(--m-bg-overlay);
		color: var(--m-fg);
	}
	.item.selected .item-title {
		font-weight: 500;
	}
	.item-icon {
		width: 14px;
		text-align: center;
		font-size: 12px;
		color: var(--m-fg-muted);
	}
	.item.selected .item-icon {
		color: var(--m-accent);
	}
	.item-title {
		flex: 1;
	}
	.item-sub {
		font-size: 11px;
		color: var(--m-fg-muted);
	}
</style>
