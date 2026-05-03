<script lang="ts">
	import { bottomPanel, type BottomPanelTab } from '../bottomPanel.svelte';
	import { container } from '../container.svelte';
	import { canOpenContainerTerminal, openContainerTerminal, openHostTerminal } from '../openTerminal';
	import { workspace } from '../state.svelte';
	import { terminalExitSuffix } from '../terminal.svelte';
	import LogTab from './LogTab.svelte';
	import TerminalTab from './TerminalTab.svelte';
	import TerminalLauncher from './TerminalLauncher.svelte';
	import TerminalTargetIcon from './TerminalTargetIcon.svelte';

	// Bottom panel region. Hosts long-lived auxiliary surfaces —
	// service-log streams and terminal sessions today, more
	// kinds later. This component owns the chrome (tab strip,
	// body shell, resize handle integration) but not the
	// rendering of any individual tab kind.
	//
	// Visibility, height, and the active tab live on
	// `bottomPanel` (`src/lib/bottomPanel.svelte.ts`); the
	// resize gesture is handled in `App.svelte` because the
	// handle sits between this region and the editor area.
	//
	// Why all bodies stay mounted
	// ---------------------------
	//
	// Tab kinds with live state (terminals own an xterm.js
	// scrollback + ANSI parser) lose data on unmount. Rather
	// than juggling per-kind serialise/replay, we render every
	// body and hide inactive ones with `display: none`. The
	// xterm Terminal stays attached; when the tab becomes
	// active again, its ResizeObserver picks up the new
	// dimensions and refits.

	const tabs = $derived(bottomPanel.tabs);
	const activeId = $derived(bottomPanel.activeId);

	function handleTabClick(id: string) {
		bottomPanel.setActive(id);
	}

	function handleClose(event: MouseEvent, id: string) {
		event.stopPropagation();
		bottomPanel.closeTab(id);
	}

	function handleHide() {
		bottomPanel.hide();
		// Bounce focus to the editor so keyboard navigation has
		// somewhere sensible to land — closing the panel via
		// keyboard would otherwise leave the body element focused
		// on a node that's about to unmount.
		workspace.requestEditorFocus();
	}

	function placeholderCopy(_tab: BottomPanelTab): string {
		return 'No content yet.';
	}

	function terminalChipFor(tab: BottomPanelTab): 'host' | 'container' | null {
		if (tab.kind === 'terminal') {
			return tab.target.kind;
		}
		return null;
	}

	function terminalTooltip(tab: BottomPanelTab): string {
		if (tab.kind !== 'terminal') {
			return '';
		}
		const where = tab.target.kind === 'host' ? 'host' : 'container';
		const cwd = tab.target.kind === 'host' ? (tab.target.cwd ?? '~') : tab.target.cwd;
		return `${where}: ${cwd}`;
	}

	const containerRunning = $derived(container.state === 'running');
	const containerDisabledReason = $derived(
		containerRunning ? 'Open terminal in container' : 'Workspace container is not running',
	);

	function quickHost() {
		openHostTerminal();
	}

	function quickContainer() {
		if (!canOpenContainerTerminal()) {
			return;
		}
		openContainerTerminal();
	}
</script>

<section class="bottom-panel" data-region="bottom-panel" tabindex="-1" aria-label="Bottom panel">
	<header class="strip">
		<ol class="tabs" role="tablist">
			{#each tabs as tab (tab.id)}
				{@const chip = terminalChipFor(tab)}
				{@const exitSuffix = tab.kind === 'terminal' ? terminalExitSuffix(tab.id) : ''}
				<li class="tab-row" class:active={tab.id === activeId} role="presentation">
					<button
						type="button"
						role="tab"
						class="tab-select"
						aria-selected={tab.id === activeId}
						title={terminalTooltip(tab)}
						onclick={() => handleTabClick(tab.id)}
					>
						{#if chip}
							<span
								class="tab-chip"
								class:chip-host={chip === 'host'}
								class:chip-container={chip === 'container'}
								aria-hidden="true"
							>
								<TerminalTargetIcon kind={chip} size={12} />
							</span>
						{/if}
						<span class="tab-title">{tab.title}</span>
						{#if exitSuffix}
							<span class="tab-exit">{exitSuffix}</span>
						{/if}
					</button>
					<button
						type="button"
						class="tab-close"
						aria-label="Close {tab.title}"
						title="Close"
						onclick={(e) => handleClose(e, tab.id)}>×</button
					>
				</li>
			{/each}
		</ol>
		<div class="strip-actions">
			<TerminalLauncher anchor="above" variant="full" title="Open a new terminal" />
			<button
				type="button"
				class="quick-btn"
				aria-label="Open terminal on host"
				title="Open terminal on host"
				onclick={quickHost}
			>
				<TerminalTargetIcon kind="host" size={14} />
			</button>
			<button
				type="button"
				class="quick-btn"
				aria-label="Open terminal in container"
				title={containerDisabledReason}
				disabled={!containerRunning}
				onclick={quickContainer}
			>
				<TerminalTargetIcon kind="container" size={14} />
			</button>
			<button type="button" class="strip-btn" title="Hide panel" aria-label="Hide bottom panel" onclick={handleHide}>
				▾
			</button>
		</div>
	</header>
	<div class="body" role="tabpanel">
		{#if tabs.length === 0}
			<p class="empty">
				No tabs open. Click <strong>+ Terminal</strong> above to open one, or click <strong>Logs</strong> on a service in
				a project popover to start streaming.
			</p>
		{:else}
			{#each tabs as tab (tab.id)}
				<div class="body-slot" class:hidden={tab.id !== activeId}>
					{#if tab.kind === 'placeholder'}
						<p class="empty">{placeholderCopy(tab)}</p>
					{:else if tab.kind === 'log'}
						<LogTab {tab} />
					{:else if tab.kind === 'terminal'}
						<TerminalTab {tab} />
					{/if}
				</div>
			{/each}
		{/if}
	</div>
</section>

<style>
	.bottom-panel {
		flex: 1;
		display: flex;
		flex-direction: column;
		min-height: 0;
		height: 100%;
		background: var(--m-bg-1);
		border-top: 1px solid var(--m-border);
		font-size: 12px;
	}
	.strip {
		display: flex;
		align-items: stretch;
		min-height: 28px;
		background: var(--m-bg-1);
		border-bottom: 1px solid var(--m-border);
	}
	.tabs {
		flex: 1;
		display: flex;
		list-style: none;
		margin: 0;
		padding: 0;
		min-width: 0;
		overflow-x: auto;
	}
	.tab-row {
		display: inline-flex;
		align-items: center;
		border-right: 1px solid var(--m-border);
		max-width: 220px;
	}
	.tab-row:hover {
		background: var(--m-bg-overlay);
	}
	.tab-row.active {
		background: var(--m-bg);
	}
	.tab-select {
		font: inherit;
		flex: 1;
		display: inline-flex;
		align-items: center;
		gap: 6px;
		min-width: 0;
		padding: 4px 4px 4px 10px;
		background: transparent;
		color: var(--m-fg-muted);
		border: none;
		cursor: pointer;
		text-align: left;
	}
	.tab-row.active .tab-select,
	.tab-row:hover .tab-select {
		color: var(--m-fg);
	}
	.tab-title {
		flex: 0 1 auto;
		min-width: 0;
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}
	.tab-exit {
		flex: 0 0 auto;
		color: var(--m-warning, #d8a657);
		font-variant-numeric: tabular-nums;
		font-size: 11px;
	}
	.tab-close {
		font: inherit;
		display: inline-flex;
		align-items: center;
		justify-content: center;
		width: 18px;
		height: 18px;
		margin-right: 4px;
		padding: 0;
		border: none;
		border-radius: 3px;
		background: transparent;
		color: var(--m-fg-subtle);
		font-size: 14px;
		line-height: 1;
		cursor: pointer;
	}
	.tab-close:hover {
		background: var(--m-bg-1);
		color: var(--m-fg);
	}
	.strip-actions {
		display: flex;
		align-items: center;
		padding: 0 4px;
		gap: 2px;
	}
	.strip-btn {
		font: inherit;
		font-size: 12px;
		line-height: 1;
		background: transparent;
		color: var(--m-fg-muted);
		border: 1px solid transparent;
		border-radius: 3px;
		padding: 2px 6px;
		cursor: pointer;
	}
	.strip-btn:hover {
		background: var(--m-bg-overlay);
		border-color: var(--m-border);
		color: var(--m-fg);
	}
	.body {
		flex: 1;
		min-height: 0;
		display: flex;
		flex-direction: column;
	}
	.body-slot {
		flex: 1;
		min-height: 0;
		display: flex;
		flex-direction: column;
	}
	.body-slot.hidden {
		display: none;
	}
	.tab-chip {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		flex-shrink: 0;
		width: 16px;
		height: 16px;
		border-radius: 3px;
		border: 1px solid var(--m-border);
		color: var(--m-fg-muted);
	}
	.tab-chip.chip-container {
		color: var(--m-success, #6ec48a);
		border-color: var(--m-success, #6ec48a);
	}
	.quick-btn {
		font: inherit;
		display: inline-flex;
		align-items: center;
		justify-content: center;
		width: 22px;
		height: 22px;
		padding: 0;
		background: transparent;
		color: var(--m-fg-muted);
		border: 1px solid transparent;
		border-radius: 3px;
		cursor: pointer;
	}
	.quick-btn:hover:not(:disabled) {
		background: var(--m-bg-overlay);
		border-color: var(--m-border);
		color: var(--m-fg);
	}
	.quick-btn:disabled {
		color: var(--m-fg-subtle);
		cursor: not-allowed;
	}
	.empty {
		margin: 0;
		padding: 8px 12px;
		color: var(--m-fg-muted);
		line-height: 1.4;
	}
</style>
