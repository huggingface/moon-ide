<script lang="ts">
	import { workspace } from '../state.svelte';

	let themeBtn: HTMLButtonElement | undefined = $state();

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
</style>
