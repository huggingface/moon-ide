<script lang="ts">
	import { bottomPanel, type BottomPanelTab } from '../bottomPanel.svelte';
	import { workspace } from '../state.svelte';

	// Bottom panel region. Hosts long-lived auxiliary surfaces —
	// service-log streams (slice 3) and, in Phase 5, terminals.
	// This component owns the chrome (tab strip, body shell,
	// resize handle integration) but not the rendering of any
	// individual tab kind: each kind renders its own body in the
	// `<svelte:component>` switch below.
	//
	// Visibility, height, and the active tab live on
	// `bottomPanel` (`src/lib/bottomPanel.svelte.ts`); the
	// resize gesture is handled in `App.svelte` because the
	// handle sits between this region and the editor area.

	const tabs = $derived(bottomPanel.tabs);
	const activeId = $derived(bottomPanel.activeId);
	const activeTab = $derived(bottomPanel.activeTab);

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
		return 'No content yet — log streams arrive in the next slice.';
	}
</script>

<section class="bottom-panel" data-region="bottom-panel" tabindex="-1" aria-label="Bottom panel">
	<header class="strip">
		<ol class="tabs" role="tablist">
			{#each tabs as tab (tab.id)}
				<li class="tab-row" class:active={tab.id === activeId} role="presentation">
					<button
						type="button"
						role="tab"
						class="tab-select"
						aria-selected={tab.id === activeId}
						onclick={() => handleTabClick(tab.id)}
					>
						<span class="tab-title">{tab.title}</span>
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
			<button type="button" class="strip-btn" title="Hide panel" aria-label="Hide bottom panel" onclick={handleHide}>
				▾
			</button>
		</div>
	</header>
	<div class="body" role="tabpanel">
		{#if activeTab}
			{#if activeTab.kind === 'placeholder'}
				<p class="empty">{placeholderCopy(activeTab)}</p>
			{/if}
		{:else}
			<p class="empty">
				<!-- Slice 3 swaps this for a "click Logs on a service row to start streaming" hint. -->
				No tabs open. Toggle this panel with Ctrl+J.
			</p>
		{/if}
	</div>
</section>

<style>
	.bottom-panel {
		display: flex;
		flex-direction: column;
		min-height: 0;
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
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
		min-width: 0;
		display: inline-block;
		max-width: 100%;
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
		overflow: auto;
		padding: 8px 12px;
	}
	.empty {
		margin: 0;
		color: var(--m-fg-muted);
		line-height: 1.4;
	}
</style>
