<script lang="ts">
	// Bottom-strip icon button + popover that lets the user pick a
	// diagnostic source to open as a tab. Mirrors the shape of
	// `TerminalLauncher.svelte` (same anchoring, same chevron-less
	// trigger style) so the two strip buttons feel consistent.
	//
	// Sources come from `diagLogs.sources`, which is the union of
	// every backend-emitted source plus everything `frontendLog`
	// has touched in this session. We always show a handful of
	// "well-known" entries even when empty so the user has a
	// discoverable starting point — picking one opens an empty
	// pane that lights up on the next emit.

	import { bottomPanel } from '../bottomPanel.svelte';
	import { diagLogs } from '../logs.svelte';

	type Props = {
		anchor?: 'above' | 'below';
		title?: string;
	};

	let { anchor = 'above', title = 'Open diagnostic logs' }: Props = $props();

	let open = $state(false);
	let rootEl: HTMLDivElement | null = null;

	/** A small allow-list of sources we always offer in the picker,
	 * even before they have any entries. Keeps the panel
	 * discoverable: the user can open `editor.completion` once
	 * and see Ctrl+Space breadcrumbs without having to hit the
	 * key first to make the source materialise. */
	const WELL_KNOWN = ['editor.completion', 'format-on-save', 'lsp.typescript', 'lsp.rust'] as const;

	const sources = $derived.by(() => {
		const seen = new Set<string>(diagLogs.sources);
		for (const s of WELL_KNOWN) {
			seen.add(s);
		}
		return [...seen].toSorted((a, b) => a.localeCompare(b));
	});

	function toggle() {
		open = !open;
	}

	function close() {
		open = false;
	}

	function pick(source: string) {
		const existing = bottomPanel.findDiagTab(source);
		if (existing) {
			bottomPanel.setActive(existing.id);
		} else {
			bottomPanel.addTab({
				id: `diag:${source}`,
				title: source,
				kind: 'diag',
				source,
			});
		}
		bottomPanel.show();
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
</script>

<svelte:window onclick={onWindowClick} />

<div class="launcher" class:above={anchor === 'above'} class:below={anchor === 'below'} bind:this={rootEl}>
	<button type="button" class="trigger" {title} aria-label="Open diagnostic logs" onclick={toggle}>
		<span class="icon" aria-hidden="true">≣</span>
		<span class="label">Logs</span>
	</button>
	{#if open}
		<div class="menu" role="menu">
			<div class="menu-head">Diagnostic source</div>
			{#each sources as source (source)}
				<button type="button" class="item" role="menuitem" onclick={() => pick(source)}>
					<span class="item-title">{source}</span>
					<span class="item-sub">{diagLogs.entriesFor(source).length} entries</span>
				</button>
			{/each}
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
		min-width: 260px;
		max-height: 340px;
		overflow-y: auto;
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
	.menu-head {
		padding: 4px 8px 2px;
		font-size: 11px;
		text-transform: uppercase;
		letter-spacing: 0.5px;
		color: var(--m-fg-subtle);
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
	.item:hover {
		background: var(--m-bg-overlay);
		border-color: var(--m-border);
	}
	.item-title {
		font-size: 12px;
		font-weight: 500;
		font-family: ui-monospace, SFMono-Regular, 'SF Mono', Menlo, Consolas, monospace;
	}
	.item-sub {
		font-size: 11px;
		color: var(--m-fg-muted);
	}
</style>
