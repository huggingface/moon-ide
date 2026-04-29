<script lang="ts">
	import { workspace } from '../state.svelte';
	import { slack } from '../slack.svelte';
	import { container, containerStateLabel } from '../container.svelte';
	import ContainerPanel from './ContainerPanel.svelte';

	let themeBtn: HTMLButtonElement | undefined = $state();
	let containerWrap: HTMLDivElement | undefined = $state();

	// Optimistic state during the two long-running ops (setup,
	// rebuild) so the pip transitions immediately rather than
	// staying on the previous glyph for a few minutes while
	// `up -d --wait` is in flight. Pause / resume / teardown are
	// quick enough that flicker isn't worth the extra branching.
	const effectiveState = $derived(
		container.inFlight === 'setup' || container.inFlight === 'rebuild' ? 'creating' : container.state,
	);

	// F6 cycle can land on the status bar; the only interactive control
	// here today is the theme toggle, so that's the focus target. If we
	// add more controls later, switch this to a generic
	// "first focusable" lookup like Sidebar.svelte does.
	$effect(() => {
		const tick = workspace.statusFocusTick;
		if (tick === 0) {
			return;
		}
		queueMicrotask(() => themeBtn?.focus());
	});

	// Click outside the popover closes it. The pip button itself is
	// inside `containerWrap`, so clicks on it are excluded from the
	// "outside" check — `togglePanel` handles open/close on the pip.
	$effect(() => {
		if (!container.panelOpen) {
			return;
		}
		const onPointerDown = (event: PointerEvent) => {
			if (containerWrap && containerWrap.contains(event.target as Node)) {
				return;
			}
			container.closePanel();
		};
		const onKey = (event: KeyboardEvent) => {
			if (event.key === 'Escape') {
				container.closePanel();
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

<div class="status" data-region="status">
	<div class="left">
		{#if workspace.workspace}
			<span class="item">{workspace.workspace.host}</span>
			<span class="item path" title={workspace.workspace.root}>
				{workspace.workspace.root}
			</span>
		{/if}
	</div>
	<div class="right">
		{#if workspace.activeFile}
			<span class="item">
				{workspace.activeFile.name}{workspace.activeFile.isDirty ? ' •' : ''}
			</span>
		{/if}
		<!-- Container status pip. Hidden until we have a status snapshot
			 (no flash of "absent" while we're still resolving the
			 active workspace at startup). Click toggles the
			 ContainerPanel popover anchored just above. -->
		{#if container.visible}
			<div class="container-wrap" bind:this={containerWrap}>
				<button
					type="button"
					class="container"
					class:active={container.panelOpen}
					title="Container: {containerStateLabel(effectiveState)}"
					onclick={() => container.togglePanel()}
				>
					<span class="pip pip-{effectiveState}"></span>
					container
				</button>
				{#if container.panelOpen}
					<ContainerPanel />
				{/if}
			</div>
		{/if}
		<!-- Chat panel toggle. Pip indicator shows connection state so
			 the user can see "Slack: connected" without opening the
			 panel. Independent dispatch from the command palette. -->
		<button
			type="button"
			class="chat"
			class:active={slack.panelVisible}
			title={slack.connected ? 'Chat (connected)' : 'Chat (not connected)'}
			onclick={() => slack.togglePanel()}
		>
			<span class="pip" class:on={slack.connected}></span>
			chat
		</button>
		<!-- Theme indicator + toggle. The label flips on every click,
			 which is also a useful diagnostic: if you click and the icon
			 doesn't change, `toggleTheme()` didn't fire; if the icon
			 changes but the colors don't, the CSS variables aren't being
			 applied. Independent dispatch path from the command palette,
			 so a broken palette doesn't hide theme state. -->
		<button
			bind:this={themeBtn}
			type="button"
			class="theme"
			title="Theme: {workspace.theme} (click to toggle)"
			onclick={() => workspace.toggleTheme()}
		>
			{workspace.theme === 'dark' ? '☾ dark' : '☀ light'}
		</button>
	</div>
</div>

<style>
	.status {
		position: fixed;
		bottom: 0;
		left: 0;
		right: 0;
		height: 24px;
		background: var(--m-bg-1);
		border-top: 1px solid var(--m-border);
		display: flex;
		align-items: center;
		justify-content: space-between;
		padding: 0 8px;
		font-size: 11px;
		color: var(--m-fg-muted);
		z-index: 10;
	}
	.left,
	.right {
		display: flex;
		align-items: center;
		gap: 12px;
		min-width: 0;
	}
	.item {
		white-space: nowrap;
		text-overflow: ellipsis;
		overflow: hidden;
	}
	.path {
		max-width: 60ch;
		color: var(--m-fg-subtle);
	}
	.theme {
		font: inherit;
		color: var(--m-fg-muted);
		background: transparent;
		border: 1px solid transparent;
		border-radius: 4px;
		padding: 0 6px;
		height: 18px;
		line-height: 18px;
		cursor: pointer;
	}
	.theme:hover {
		background: var(--m-bg-overlay);
		color: var(--m-fg);
	}
	.chat {
		font: inherit;
		color: var(--m-fg-muted);
		background: transparent;
		border: 1px solid transparent;
		border-radius: 4px;
		padding: 0 6px;
		height: 18px;
		line-height: 18px;
		display: flex;
		align-items: center;
		gap: 5px;
		cursor: pointer;
	}
	.chat:hover {
		background: var(--m-bg-overlay);
		color: var(--m-fg);
	}
	.chat.active {
		color: var(--m-fg);
	}
	.container-wrap {
		position: relative;
		display: flex;
		align-items: center;
	}
	.container {
		font: inherit;
		color: var(--m-fg-muted);
		background: transparent;
		border: 1px solid transparent;
		border-radius: 4px;
		padding: 0 6px;
		height: 18px;
		line-height: 18px;
		display: flex;
		align-items: center;
		gap: 5px;
		cursor: pointer;
	}
	.container:hover {
		background: var(--m-bg-overlay);
		color: var(--m-fg);
	}
	.container.active {
		color: var(--m-fg);
		background: var(--m-bg-overlay);
	}
	.pip {
		display: inline-block;
		width: 6px;
		height: 6px;
		border-radius: 50%;
		background: var(--m-fg-subtle);
	}
	.pip.on {
		background: var(--m-success);
	}
	/* Container pip colour-codes the high-level state. Same palette
	   as the ContainerPanel header so the two read as one signal. */
	.pip-absent {
		background: var(--m-fg-subtle);
	}
	.pip-creating {
		background: var(--m-warning, #d4a017);
		animation: pulse 1.6s ease-in-out infinite;
	}
	.pip-running {
		background: var(--m-success);
	}
	.pip-paused {
		background: var(--m-fg-muted);
		box-shadow: inset 0 0 0 1px var(--m-fg-subtle);
	}
	.pip-stopped {
		background: var(--m-fg-subtle);
		box-shadow: inset 0 0 0 1px var(--m-fg-muted);
	}
	.pip-failed {
		background: var(--m-danger);
	}
	@keyframes pulse {
		0%,
		100% {
			opacity: 1;
		}
		50% {
			opacity: 0.4;
		}
	}
</style>
