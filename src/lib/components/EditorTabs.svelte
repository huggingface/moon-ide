<script lang="ts">
	import { mount as mountComponent, unmount } from 'svelte';
	import { workspace, type MarkdownView, type OpenFile, type SplitSide } from '../state.svelte';
	import { isMarkdownPath } from '../util/markdown';
	import { isReviewPath } from '../util/reviewPath';
	import ContextMenu from './ContextMenu.svelte';
	import type { ContextMenuItem } from './contextMenu';
	import RevertIcon from './icons/RevertIcon.svelte';

	type Props = { side: SplitSide };
	let { side }: Props = $props();

	const activePath: string | null = $derived(side === 'left' ? workspace.leftActive : workspace.rightActive);
	// When split, both panes show an active tab. The accent underline
	// reads as "where typing goes" — we want only the focused pane to
	// claim it. The other pane keeps its tab marked active but with a
	// muted underline so the user can still tell which tab is active
	// over there.
	const paneFocused = $derived(workspace.focusedSide === side);
	// Tab list is per pane (Phase 1.5): the strip renders only the
	// paths assigned to this side, in this side's order.
	const tabPaths: string[] = $derived(workspace.tabsFor(side));
	const tabs: OpenFile[] = $derived(
		tabPaths.flatMap((p) => {
			const file = workspace.openFiles.find((f) => f.path === p);
			return file ? [file] : [];
		}),
	);
	// Right-edge view toggle: a single tri-state group covering
	// `Source` / `Preview` / `Diff`. Per-buffer (not per-pane), see
	// `WorkspaceState.previewModeFor` and `diffModeFor` for the
	// rationale. Buttons that don't apply to the active buffer
	// drop out of the strip.
	type ViewMode = 'source' | 'preview' | 'diff';

	const activeFile = $derived.by(() => {
		if (activePath === null) {
			return null;
		}
		return workspace.openFiles.find((f) => f.path === activePath) ?? null;
	});
	const activeIsMarkdown = $derived(activePath !== null && isMarkdownPath(activePath));
	const previewMode: MarkdownView = $derived(activePath !== null ? workspace.previewModeFor(activePath) : 'source');
	const diffMode: boolean = $derived(activePath !== null && workspace.diffModeFor(activePath));
	const activeIsDeleted = $derived(activeFile?.kind === 'text' && activeFile.isDeleted);
	// "Diff" button is meaningful only when there's something to
	// diff: a tracked file with working-tree differences
	// (`modified`). Untracked / added / clean / ignored / image
	// buffers don't surface it — no `HEAD` side worth rendering.
	// Deleted buffers don't surface it either: their normal view
	// is a read-only `Editor` of the HEAD blob (see
	// `Editor.baseExtensions` and `EditorPane.showDiff`), and
	// the explicit "View diff" right-click in the file tree is
	// the path to the side-by-side for the rare "show me HEAD
	// vs empty" use case — putting the same flip in the tab
	// toolbar would just be a noisy "Source" button most of the
	// time.
	const canDiff = $derived.by(() => {
		if (activeFile === null || activeFile.kind !== 'text') {
			return false;
		}
		if (activeFile.isUntitled || activeFile.isDeleted) {
			return false;
		}
		const status = workspace.gitStatusEntries.find((e) => e.path === activePath)?.status;
		return status === 'modified';
	});
	const currentView: ViewMode = $derived(diffMode ? 'diff' : previewMode === 'preview' ? 'preview' : 'source');
	const showViewToggle = $derived(!activeIsDeleted && (activeIsMarkdown || canDiff));
	// Revert icon: shows whenever there's a HEAD state to fall back
	// to (modified or deleted). Untracked / added / clean don't get
	// the icon — for those, "revert" either means trashing the file
	// (file-tree menu still offers it) or doesn't apply. The icon
	// rides next to the view toggle and brings up the same confirm
	// dialog as the file-tree's "Discard changes" entry.
	const canRevert = $derived.by(() => {
		if (activeFile === null || activeFile.kind !== 'text') {
			return false;
		}
		if (activeFile.isUntitled) {
			return false;
		}
		const status = workspace.gitStatusEntries.find((e) => e.path === activePath)?.status;
		return status === 'modified' || status === 'deleted';
	});
	const showToolbar = $derived(showViewToggle || canRevert);

	function setView(mode: ViewMode) {
		if (activePath === null) {
			return;
		}
		// `diff` is exclusive with markdown preview: showDiff wins
		// inside `EditorPane`, but we also flip `previewMode` to
		// `source` when leaving diff so a re-enter into "Source"
		// (from Diff) doesn't accidentally land in Preview just
		// because that was the mode before.
		if (mode === 'diff') {
			workspace.setDiffMode(activePath, true);
			return;
		}
		workspace.setDiffMode(activePath, false);
		workspace.setPreviewMode(activePath, mode === 'preview' ? 'preview' : 'source');
	}

	function revertActive() {
		if (activePath === null) {
			return;
		}
		void workspace.discardPaths([activePath]);
	}

	// MIME type used to identify our own tab drags. The side payload
	// lets the drop target know which pane the tab came from (so it
	// can call `moveTab` with the right source); only readable on
	// drop, but `dataTransfer.types` is readable in `dragover`, so
	// the TAB_MIME entry doubles as the "is this our drag?" gate.
	const TAB_MIME = 'application/x-moon-tab';
	const TAB_SIDE_MIME = 'application/x-moon-tab-side';

	let draggingPath = $state<string | null>(null);
	let dropBeforePath = $state<string | null>(null);
	// Tracks the drop position when the cursor is past the last tab. We
	// can't read the source path during `dragover` to early-out for
	// "dropping on yourself when you're already last", so we just always
	// allow it and noop in `moveFile` when the move is a no-op.
	let dropAtEnd = $state(false);

	// Strip element so we can scroll the active tab into view when
	// the user opens / focuses a file from elsewhere (file tree,
	// command palette, "go to definition", etc). Without this, a
	// long-running session where the user has 30 tabs open ends up
	// switching to a file whose tab is two screens off and the
	// horizontal scroll position never catches up.
	let tabsEl: HTMLDivElement | undefined = $state(undefined);
	$effect(() => {
		// Track both deps explicitly so the effect re-runs on
		// activate-different-tab and on reorder (drag, close).
		activePath;
		tabPaths;
		if (!tabsEl || activePath === null) {
			return;
		}
		// Defer one frame so the just-mounted tab is in the DOM
		// when we look it up — `$effect` runs after the current
		// flush, but the tab node for a freshly-opened file is
		// only attached during that same flush, and on cold open
		// (first render) `querySelector` would otherwise miss it.
		const handle = requestAnimationFrame(() => {
			const el = tabsEl?.querySelector<HTMLElement>('[role="tab"][aria-selected="true"]');
			if (el) {
				el.scrollIntoView({ block: 'nearest', inline: 'nearest' });
			}
		});
		return () => cancelAnimationFrame(handle);
	});

	function close(event: Event, path: string) {
		event.stopPropagation();
		void workspace.closeFile(path, side);
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
		event.dataTransfer.setData(TAB_SIDE_MIME, side);
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
		const idx = tabPaths.indexOf(path);
		const next = idx >= 0 ? tabPaths[idx + 1] : undefined;
		if (next) {
			dropBeforePath = next;
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
		const fromSideRaw = event.dataTransfer?.getData(TAB_SIDE_MIME) ?? '';
		const target = dropAtEnd ? null : dropBeforePath;
		dropBeforePath = null;
		dropAtEnd = false;
		draggingPath = null;
		if (fromPath === '') {
			return;
		}
		// Older drags or drops from a non-tab source may not carry the
		// side payload — fall back to "same side" so we just reorder.
		const fromSide: SplitSide = fromSideRaw === 'left' || fromSideRaw === 'right' ? fromSideRaw : side;
		workspace.moveTab(fromPath, fromSide, side, target);
	}

	function onDragEnd() {
		draggingPath = null;
		dropBeforePath = null;
		dropAtEnd = false;
	}

	// Right-click on a tab opens a small action menu. We re-use
	// `ContextMenu.svelte` (the same component the file tree mounts
	// for its row menus) by spawning a portaled host on `document.body`
	// and tearing it down on close. Going through `mount` instead of
	// rendering the menu inside the strip keeps it from getting clipped
	// by the tab strip's `overflow: hidden` and lets the popover
	// position itself against the viewport.
	let activeTabMenu: ReturnType<typeof mountComponent> | null = null;
	let activeTabMenuHost: HTMLElement | null = null;

	$effect(() => {
		return () => {
			disposeTabMenu();
		};
	});

	function disposeTabMenu() {
		if (activeTabMenu) {
			void unmount(activeTabMenu);
			activeTabMenu = null;
		}
		if (activeTabMenuHost) {
			activeTabMenuHost.remove();
			activeTabMenuHost = null;
		}
	}

	function absolutePathFor(file: OpenFile): string | null {
		if (file.isUntitled || isReviewPath(file.path)) {
			return null;
		}
		if (file.isExternal) {
			return file.path;
		}
		const root = workspace.activeFolderPath;
		if (root === null) {
			return null;
		}
		return `${root.replace(/\/+$/, '')}/${file.path}`;
	}

	async function copyToClipboard(text: string, label: string) {
		try {
			await navigator.clipboard.writeText(text);
			workspace.flash(`Copied ${label}`);
		} catch {
			workspace.flash(`Could not copy ${label}`);
		}
	}

	function buildTabMenuItems(file: OpenFile): ContextMenuItem[] {
		const items: ContextMenuItem[] = [];
		const absolute = absolutePathFor(file);
		const copyPathItem: ContextMenuItem = {
			id: 'copy-path',
			label: 'Copy path',
			disabled: absolute === null,
			onSelect: () => {
				if (absolute !== null) {
					void copyToClipboard(absolute, 'path');
				}
			},
		};
		if (absolute !== null) {
			copyPathItem.title = absolute;
		}
		items.push(copyPathItem);
		// "Relative path" only makes sense for in-folder files. For
		// external buffers `file.path` already *is* the absolute host
		// path, so the relative entry would be a duplicate of "Copy
		// path"; for untitled buffers there's no path at all.
		if (!file.isExternal && !file.isUntitled && !isReviewPath(file.path)) {
			items.push({
				id: 'copy-relative-path',
				label: 'Copy relative path',
				title: file.path,
				onSelect: () => {
					void copyToClipboard(file.path, 'relative path');
				},
			});
		}

		const paneTabs = workspace.tabsFor(side);
		const hasOthers = paneTabs.some((p) => p !== file.path);
		items.push({
			id: 'close',
			label: 'Close',
			onSelect: () => {
				void workspace.closeFile(file.path, side);
			},
		});
		items.push({
			id: 'close-others',
			label: 'Close others',
			disabled: !hasOthers,
			onSelect: () => {
				const others = workspace.tabsFor(side).filter((p) => p !== file.path);
				for (const p of others) {
					void workspace.closeFile(p, side);
				}
			},
		});
		items.push({
			id: 'close-all',
			label: 'Close all',
			disabled: paneTabs.length === 0,
			onSelect: () => {
				const all = workspace.tabsFor(side).slice();
				for (const p of all) {
					void workspace.closeFile(p, side);
				}
			},
		});
		return items;
	}

	function openTabMenu(event: MouseEvent, file: OpenFile) {
		event.preventDefault();
		event.stopPropagation();
		disposeTabMenu();

		const items = buildTabMenuItems(file);
		const host = document.createElement('div');
		host.setAttribute('data-tab-context-menu-root', 'true');
		host.style.position = 'fixed';
		host.style.top = '0';
		host.style.left = '0';
		host.style.width = '0';
		host.style.height = '0';
		host.style.zIndex = '9999';
		document.body.appendChild(host);

		const anchorRect = { left: event.clientX, top: event.clientY, width: 0, height: 0 };
		activeTabMenu = mountComponent(ContextMenu, {
			target: host,
			props: {
				items,
				anchorRect,
				onClose: () => {
					disposeTabMenu();
				},
			},
		});
		activeTabMenuHost = host;
	}
</script>

<!--
	The tablist itself isn't tab-focusable (`tabindex="-1"`) because focus
	per the WAI-ARIA tablist pattern lives on the active `role="tab"`,
	not the strip container. We still need to keep the attribute present
	to satisfy svelte-check now that the strip carries `ondragover`/
	`ondrop` (which classify it as interactive).
-->
<div class="strip">
	<div
		bind:this={tabsEl}
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
		{#each tabs as file (file.path)}
			<div
				role="tab"
				class="tab"
				class:active={activePath === file.path}
				class:active-blurred={activePath === file.path && !paneFocused}
				class:dragging={draggingPath === file.path}
				class:drop-before={dropBeforePath === file.path}
				aria-selected={activePath === file.path}
				title={file.isUntitled || isReviewPath(file.path) ? file.name : file.path}
				tabindex="0"
				draggable="true"
				onclick={() => workspace.setActive(file.path, side)}
				onkeydown={(e) => onTabKey(e, file.path)}
				oncontextmenu={(e) => openTabMenu(e, file)}
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
	{#if showToolbar}
		<div class="view-toggle" role="group" aria-label="View mode">
			{#if showViewToggle}
				<button
					type="button"
					class="view-btn"
					class:selected={currentView === 'source'}
					aria-pressed={currentView === 'source'}
					onclick={() => setView('source')}
				>
					Source
				</button>
				{#if activeIsMarkdown}
					<button
						type="button"
						class="view-btn"
						class:selected={currentView === 'preview'}
						aria-pressed={currentView === 'preview'}
						onclick={() => setView('preview')}
					>
						Preview
					</button>
				{/if}
				{#if canDiff}
					<button
						type="button"
						class="view-btn"
						class:selected={currentView === 'diff'}
						aria-pressed={currentView === 'diff'}
						onclick={() => setView('diff')}
						title="Show diff against HEAD (Ctrl+Shift+D)"
					>
						Diff
					</button>
				{/if}
			{/if}
			{#if canRevert}
				<button
					type="button"
					class="view-icon-btn"
					onclick={revertActive}
					title={activeIsDeleted ? 'Restore file from HEAD' : 'Revert file to HEAD'}
					aria-label={activeIsDeleted ? 'Restore file from HEAD' : 'Revert file to HEAD'}
				>
					<RevertIcon />
				</button>
			{/if}
		</div>
	{/if}
</div>

<style>
	.strip {
		display: flex;
		align-items: stretch;
		height: 32px;
		background: var(--m-bg-1);
		border-bottom: 1px solid var(--m-border);
		flex-shrink: 0;
	}
	.tabs {
		display: flex;
		align-items: stretch;
		flex: 1;
		min-width: 0;
		overflow-x: auto;
		overflow-y: hidden;
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
	/* Source/Preview toggle, anchored to the right end of the tab
	strip whenever the active tab is markdown. Pure UI affordance; the
	state lives on the buffer (per-path), so the same file in two
	panes shows the same mode. */
	.view-toggle {
		display: flex;
		align-items: center;
		gap: 2px;
		padding: 4px 8px;
		flex-shrink: 0;
		border-left: 1px solid var(--m-border);
	}
	.view-btn {
		font-family: var(--m-font-ui);
		font-size: 11px;
		font-weight: 500;
		color: var(--m-fg-muted);
		background: transparent;
		border: 1px solid transparent;
		border-radius: 3px;
		padding: 2px 8px;
		cursor: pointer;
		user-select: none;
		-webkit-user-select: none;
	}
	.view-btn:hover {
		color: var(--m-fg);
		background: var(--m-bg-overlay);
	}
	.view-btn.selected {
		color: var(--m-fg);
		background: var(--m-bg-3);
		border-color: var(--m-border);
	}
	/* Icon-only button alongside the Source/Diff text buttons. Same
	   hit-target height as `.view-btn` (24px = 11px text + 6px+6px pad
	   + 1+1px border equivalents) so the row stays aligned, but a
	   square footprint instead of text padding. */
	.view-icon-btn {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		width: 22px;
		height: 22px;
		margin-left: 4px;
		color: var(--m-fg-muted);
		background: transparent;
		border: 1px solid transparent;
		border-radius: 3px;
		padding: 0;
		cursor: pointer;
		user-select: none;
		-webkit-user-select: none;
	}
	.view-icon-btn:hover {
		color: var(--m-fg);
		background: var(--m-bg-overlay);
	}
	.view-icon-btn:active {
		background: var(--m-bg-3);
	}
</style>
