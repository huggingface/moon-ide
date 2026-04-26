<script lang="ts">
	import { workspace, type SplitSide } from '../state.svelte';

	type Props = { side: SplitSide };
	let { side }: Props = $props();

	const activePath: string | null = $derived(side === 'left' ? workspace.leftActive : workspace.rightActive);
	// When split, both panes show an active tab. The accent underline
	// reads as "where typing goes" — we want only the focused pane to
	// claim it. The other pane keeps its tab marked active but with a
	// muted underline so the user can still tell which tab is active
	// over there.
	const paneFocused = $derived(workspace.focusedSide === side);

	// MIME type used to identify our own tab drags. Reading the actual
	// payload is restricted to the `drop` handler by browser policy, but
	// `dataTransfer.types` is readable in `dragover` — we use that to
	// reject drags that didn't start in our tab strip (random files
	// dropped from outside).
	const TAB_MIME = 'application/x-moon-tab';

	let draggingPath = $state<string | null>(null);
	let dropBeforePath = $state<string | null>(null);
	// Tracks the drop position when the cursor is past the last tab. We
	// can't read the source path during `dragover` to early-out for
	// "dropping on yourself when you're already last", so we just always
	// allow it and noop in `moveFile` when the move is a no-op.
	let dropAtEnd = $state(false);

	function close(event: Event, path: string) {
		event.stopPropagation();
		void workspace.closeFile(path);
	}

	function onTabKey(event: KeyboardEvent, path: string) {
		if (event.key === 'Enter' || event.key === ' ') {
			event.preventDefault();
			workspace.setActive(path, side);
		}
	}

	function isTabDrag(event: DragEvent): boolean {
		const types = event.dataTransfer?.types;
		if (!types) {
			return false;
		}
		for (const t of types) {
			if (t === TAB_MIME) {
				return true;
			}
		}
		return false;
	}

	function onTabDragStart(event: DragEvent, path: string) {
		if (!event.dataTransfer) {
			return;
		}
		event.dataTransfer.effectAllowed = 'move';
		event.dataTransfer.setData(TAB_MIME, path);
		// Plain-text fallback so dragging a tab into a text field does
		// something sensible instead of silently failing.
		event.dataTransfer.setData('text/plain', path);
		draggingPath = path;
	}

	function onTabDragOver(event: DragEvent, path: string) {
		if (!isTabDrag(event)) {
			return;
		}
		event.preventDefault();
		if (event.dataTransfer) {
			event.dataTransfer.dropEffect = 'move';
		}
		// Decide drop side based on cursor position relative to the
		// hovered tab's midpoint. Hovering the left half drops *before*
		// this tab; hovering the right half drops before the next tab
		// (effectively "after" this one).
		const target = event.currentTarget as HTMLElement;
		const rect = target.getBoundingClientRect();
		const before = event.clientX < rect.left + rect.width / 2;
		if (before) {
			dropBeforePath = path;
			dropAtEnd = false;
			return;
		}
		const idx = workspace.openFiles.findIndex((f) => f.path === path);
		const next = workspace.openFiles[idx + 1];
		if (next) {
			dropBeforePath = next.path;
			dropAtEnd = false;
			return;
		}
		dropBeforePath = null;
		dropAtEnd = true;
	}

	function onStripDragOver(event: DragEvent) {
		if (event.target !== event.currentTarget) {
			return;
		}
		if (!isTabDrag(event)) {
			return;
		}
		event.preventDefault();
		if (event.dataTransfer) {
			event.dataTransfer.dropEffect = 'move';
		}
		dropBeforePath = null;
		dropAtEnd = true;
	}

	function onDrop(event: DragEvent) {
		if (!isTabDrag(event)) {
			return;
		}
		event.preventDefault();
		const fromPath = event.dataTransfer?.getData(TAB_MIME) ?? '';
		const target = dropAtEnd ? null : dropBeforePath;
		dropBeforePath = null;
		dropAtEnd = false;
		draggingPath = null;
		if (fromPath === '') {
			return;
		}
		workspace.moveFile(fromPath, target);
	}

	function onDragEnd() {
		draggingPath = null;
		dropBeforePath = null;
		dropAtEnd = false;
	}
</script>

<!--
	The tablist itself isn't tab-focusable (`tabindex="-1"`) because focus
	per the WAI-ARIA tablist pattern lives on the active `role="tab"`,
	not the strip container. We still need to keep the attribute present
	to satisfy svelte-check now that the strip carries `ondragover`/
	`ondrop` (which classify it as interactive).
-->
<div
	class="tabs"
	class:drop-end={dropAtEnd}
	role="tablist"
	tabindex="-1"
	ondragover={onStripDragOver}
	ondrop={onDrop}
	ondragleave={() => {
		dropBeforePath = null;
		dropAtEnd = false;
	}}
>
	{#each workspace.openFiles as file (file.path)}
		<div
			role="tab"
			class="tab"
			class:active={activePath === file.path}
			class:active-blurred={activePath === file.path && !paneFocused}
			class:dragging={draggingPath === file.path}
			class:drop-before={dropBeforePath === file.path}
			aria-selected={activePath === file.path}
			title={file.path}
			tabindex="0"
			draggable="true"
			onclick={() => workspace.setActive(file.path, side)}
			onkeydown={(e) => onTabKey(e, file.path)}
			ondragstart={(e) => onTabDragStart(e, file.path)}
			ondragover={(e) => onTabDragOver(e, file.path)}
			ondragend={onDragEnd}
		>
			<span class="name">{file.name}</span>
			{#if file.isDirty}
				<span class="dirty" aria-label="unsaved changes">●</span>
			{/if}
			<button type="button" class="close" aria-label="Close tab" onclick={(e) => close(e, file.path)}>×</button>
		</div>
	{/each}
</div>

<style>
	.tabs {
		display: flex;
		align-items: stretch;
		height: 32px;
		background: var(--m-bg-1);
		border-bottom: 1px solid var(--m-border);
		overflow-x: auto;
		overflow-y: hidden;
		flex-shrink: 0;
		position: relative;
		/* Hide the scrollbar entirely. The native (GTK/WebKit2) bar grew
		on hover and stole the tab's bottom 4px every time the cursor
		passed near the strip — too annoying for the gain. Wheel /
		touch scrolling still work. If we ever have so many tabs that
		this becomes a discoverability issue we'll add an overflow
		menu, not the bar back. */
		scrollbar-width: none;
	}
	.tabs::-webkit-scrollbar {
		display: none;
	}
	.tab {
		display: inline-flex;
		align-items: center;
		gap: 6px;
		padding: 0 8px 0 12px;
		border: none;
		border-right: 1px solid var(--m-border);
		border-radius: 0;
		background: transparent;
		color: var(--m-fg-muted);
		font-size: 12px;
		cursor: pointer;
		white-space: nowrap;
		height: 100%;
		position: relative;
		/* Click-and-drag should reorder the tab, not select its label. */
		user-select: none;
		-webkit-user-select: none;
	}
	.tab:hover {
		color: var(--m-fg);
		background: var(--m-bg-overlay);
	}
	.tab.active {
		background: var(--m-bg);
		color: var(--m-fg);
		box-shadow: inset 0 -2px 0 var(--m-accent);
	}
	/* Same tab is "active" in the unfocused split: keep the body
	highlighted (so the user can still tell which tab is current over
	there) but mute the accent underline — only the focused pane owns
	the "where typing goes" signal. */
	.tab.active-blurred {
		box-shadow: inset 0 -2px 0 var(--m-fg-subtle);
		color: var(--m-fg-muted);
	}
	.tab.dragging {
		opacity: 0.5;
	}
	/* Drop position indicator: a vertical accent stripe at the tab's
	leading edge for an "insert before this tab" drop. The trailing
	"drop at end of strip" case lives on the strip itself. */
	.tab.drop-before::before {
		content: '';
		position: absolute;
		top: 0;
		bottom: 0;
		left: -1px;
		width: 2px;
		background: var(--m-accent);
		pointer-events: none;
	}
	.tabs.drop-end::after {
		content: '';
		flex: 0 0 2px;
		align-self: stretch;
		background: var(--m-accent);
	}
	.name {
		font-family: var(--m-font-ui);
	}
	.dirty {
		color: var(--m-warning);
		font-size: 10px;
		line-height: 1;
	}
	.close {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		width: 16px;
		height: 16px;
		border-radius: 3px;
		color: var(--m-fg-subtle);
		font-size: 14px;
		line-height: 1;
		background: transparent;
		border: none;
		padding: 0;
	}
	.close:hover {
		background: var(--m-bg-3);
		color: var(--m-fg);
	}
</style>
