<script lang="ts">
	// Host/container terminal launcher. When the workspace
	// container is running, trigger opens a two-item menu;
	// when not, trigger opens a host terminal directly.
	// Reused from the bottom-panel strip and the status bar.
	// Picks sensible defaults from current workspace
	// state:
	//
	//   * Host terminal cwd = active folder's host path; `null`
	//     (= `$HOME`) when no folder is bound.
	//   * Container terminal cwd = `/workspace/<basename>` for
	//     the active folder, `/workspace` as a fallback. If the
	//     container stops while the menu is open, "In container"
	//     becomes disabled.
	//
	// Architecture: ADR 0009.

	import { container } from '../container.svelte';
	import { workspace } from '../state.svelte';
	import { containerCwdFor, openContainerTerminal, openHostTerminal } from '../openTerminal';

	type Props = {
		// Affects layout only — the popover anchors above the
		// trigger by default; "below" flips it for status-bar use.
		anchor?: 'above' | 'below';
		// Optional tooltip override on the trigger.
		title?: string;
		// Render style of the trigger. `compact` is the small
		// icon-style button used in the status bar; `full` is
		// the wider "+ Terminal" button used in the panel strip.
		variant?: 'compact' | 'full';
	};

	let { anchor = 'above', title = 'Open terminal', variant = 'full' }: Props = $props();

	let open = $state(false);

	const containerRunning = $derived(container.state === 'running');
	const containerDisabledReason = $derived(
		containerRunning ? null : 'Workspace container is not running. Start it from the status bar.',
	);
	const activeFolder = $derived(workspace.activeFolder);

	async function toggle() {
		// Wait for the startup `container.refresh()` to settle so
		// a click that lands during the cold-launch probe doesn't
		// see a stale `null` state and silently open a host
		// terminal. After the await `containerRunning` reads
		// whatever the daemon actually says.
		await container.awaitRefreshed();
		if (!containerRunning) {
			openHostTerminal();
			return;
		}
		open = !open;
	}

	function close() {
		open = false;
	}

	function pickHost() {
		openHostTerminal();
		close();
	}

	function pickContainer() {
		if (!containerRunning) {
			return;
		}
		openContainerTerminal();
		close();
	}

	function onWindowClick(event: MouseEvent) {
		if (!open) {
			return;
		}
		const target = event.target as Node | null;
		if (target && (rootEl?.contains(target) ?? false)) {
			return;
		}
		close();
	}

	let rootEl: HTMLDivElement | null = null;
</script>

<svelte:window onclick={onWindowClick} />

<div class="launcher" class:above={anchor === 'above'} class:below={anchor === 'below'} bind:this={rootEl}>
	<button
		type="button"
		class="trigger"
		class:compact={variant === 'compact'}
		{title}
		aria-label="Open terminal"
		onclick={toggle}
	>
		{#if variant === 'compact'}
			<span class="icon" aria-hidden="true">▣</span>
		{:else}
			<span class="icon" aria-hidden="true">+</span>
			<span class="label">Terminal</span>
		{/if}
	</button>
	{#if open}
		<div class="menu" role="menu">
			<button type="button" class="item" role="menuitem" onclick={pickHost}>
				<span class="item-title">On host</span>
				<span class="item-sub">{activeFolder ? activeFolder.path : '~'}</span>
			</button>
			<button
				type="button"
				class="item"
				role="menuitem"
				disabled={!containerRunning}
				title={containerDisabledReason ?? ''}
				onclick={pickContainer}
			>
				<span class="item-title">In container</span>
				<span class="item-sub">{activeFolder ? containerCwdFor(activeFolder.path) : '/workspace'}</span>
			</button>
		</div>
	{/if}
</div>

<style>
	.launcher {
		position: relative;
		display: inline-flex;
	}
	.trigger {
		font: inherit;
		font-size: 12px;
		line-height: 1;
		display: inline-flex;
		align-items: center;
		gap: 4px;
		background: transparent;
		color: var(--m-fg-muted);
		border: 1px solid transparent;
		border-radius: 3px;
		padding: 2px 8px;
		cursor: pointer;
	}
	.trigger:hover {
		background: var(--m-bg-overlay);
		border-color: var(--m-border);
		color: var(--m-fg);
	}
	.trigger.compact {
		padding: 2px 6px;
	}
	.icon {
		font-size: 12px;
		line-height: 1;
	}
	.label {
		font-size: 12px;
	}
	.menu {
		position: absolute;
		right: 0;
		min-width: 220px;
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
	.above .menu {
		bottom: 100%;
		margin-bottom: 6px;
	}
	.below .menu {
		top: 100%;
		margin-top: 6px;
	}
	.item {
		font: inherit;
		display: flex;
		flex-direction: column;
		align-items: flex-start;
		gap: 2px;
		padding: 6px 8px;
		background: transparent;
		color: var(--m-fg);
		border: 1px solid transparent;
		border-radius: 4px;
		cursor: pointer;
		text-align: left;
	}
	.item:hover:not(:disabled) {
		background: var(--m-bg-overlay);
		border-color: var(--m-border);
	}
	.item:disabled {
		color: var(--m-fg-subtle);
		cursor: not-allowed;
	}
	.item-title {
		font-size: 12px;
		font-weight: 500;
	}
	.item-sub {
		font-size: 11px;
		color: var(--m-fg-muted);
		font-family: ui-monospace, SFMono-Regular, 'SF Mono', Menlo, Consolas, monospace;
		max-width: 100%;
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}
</style>
