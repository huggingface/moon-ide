<script lang="ts">
	import { onMount, tick, untrack } from 'svelte';
	import { FileTree } from '@pierre/trees';
	import { workspace } from '../state.svelte';

	let mount: HTMLDivElement;
	let tree: FileTree | undefined;

	onMount(() => {
		if (!mount) {
			return;
		}
		tree = new FileTree({
			paths: untrack(() => workspace.paths),
			flattenEmptyDirectories: true,
			initialExpansion: 1,
			search: true,
			onSelectionChange: (selectedPaths) => {
				if (selectedPaths.length === 0) {
					return;
				}
				const path = selectedPaths[0];
				if (path === undefined) {
					return;
				}
				const item = tree?.getItem(path);
				if (!item || item.isDirectory()) {
					return;
				}
				// Preview-open the file but keep DOM focus inside the tree
				// so the user can keep arrowing/clicking through siblings
				// without every selection yanking the caret into the
				// editor. Enter or double-click hands focus over (see
				// handlers below).
				void workspace.openFile(path, undefined, { focus: false });
			},
		});
		tree.render({ containerWrapper: mount });
		return () => {
			tree?.cleanUp();
			tree = undefined;
		};
	});

	// Reactively reset paths when the workspace path list changes.
	$effect(() => {
		const paths = workspace.paths;
		if (!tree) {
			return;
		}
		tree.resetPaths(paths);
	});

	// Mirror the active file in the tree's selection so the row stays
	// highlighted as the user switches tabs (or restores a session). Two
	// invariants:
	//   1. If a file is active, exactly that file's row is selected.
	//   2. If no file is active (closed last tab, fresh workspace), the
	//      selection is cleared — leaving a stale row selected makes
	//      re-clicking the same row a no-op (Pierre only fires
	//      `onSelectionChange` on real changes).
	// We early-return when already in sync so a user click (which already
	// produces the desired selection via Pierre) doesn't trigger a
	// feedback loop through programmatic `select()`.
	$effect(() => {
		const target = workspace.activePath;
		if (!tree) {
			return;
		}
		const current = tree.getSelectedPaths();
		const alreadyInSync = target === null ? current.length === 0 : current.length === 1 && current[0] === target;
		if (alreadyInSync) {
			return;
		}
		for (const sel of current) {
			if (sel !== target) {
				tree.getItem(sel)?.deselect();
			}
		}
		if (target !== null) {
			tree.getItem(target)?.select();
			void scrollPathIntoView(tree, target);
		}
	});

	// Bring `path` into the tree's viewport, regardless of whether DOM focus
	// currently lives in the tree. Pierre virtualizes rows aggressively, so
	// we have to coax its renderer to *put the row into the DOM* before we
	// can call `scrollIntoView` on it. Strategy in three layers:
	//   1. If the row is already mounted (overscan, partial visibility),
	//      call `scrollIntoView({ block: 'nearest' })` and we're done.
	//   2. Otherwise, ask the controller to focus the path with
	//      `focusNearestPath`. This is the same call Pierre uses on click;
	//      it resolves through collapsed ancestors, sets `#focusedIndex`,
	//      and on the resulting render Pierre's layout effect runs
	//      `scrollFocusedRowIntoView` *iff DOM focus is inside the tree*.
	//      To satisfy that "iff", we briefly park focus on the scroll
	//      container (`tabindex=-1` is a one-frame escape hatch) and
	//      restore the previous focus once Pierre's commit settles.
	//   3. After the autoscroll, the row is now in the rendered window;
	//      retry `scrollIntoView` to absorb any sub-pixel offset Pierre's
	//      compute formula left on the table.
	async function scrollPathIntoView(local: FileTree, path: string) {
		await tick();
		if (tryDirectScroll(local, path)) {
			return;
		}
		await new Promise<void>((resolve) => requestAnimationFrame(() => resolve()));
		if (tryDirectScroll(local, path)) {
			return;
		}

		const root = local.getFileTreeContainer()?.shadowRoot;
		if (!root) {
			return;
		}
		const scrollEl = root.querySelector<HTMLElement>('[data-file-tree-virtualized-scroll]');
		if (!scrollEl) {
			return;
		}

		const previousFocus = getDeepActiveElement();
		const hadTabIndex = scrollEl.hasAttribute('tabindex');
		if (!hadTabIndex) {
			scrollEl.setAttribute('tabindex', '-1');
		}

		// Order matters: focus BEFORE asking Pierre to update its focused
		// path. The view's `useLayoutEffect` reads `shadowRoot.activeElement`
		// synchronously and gates `scrollFocusedRowIntoView` on
		// `shouldOwnDomFocus && focusedPathChanged`. Doing focus first means
		// the layout effect that runs after `focusNearestPath`'s emit sees
		// both flags true on the same pass.
		scrollEl.focus({ preventScroll: true });
		local.focusNearestPath(path);

		await tick();
		await new Promise<void>((resolve) => requestAnimationFrame(() => resolve()));

		// Pierre's autoscroll uses `computeFocusedRowScrollIntoView` which
		// only adjusts `scrollTop` when the focused index falls outside the
		// viewport. The row should now be mounted; a final `scrollIntoView`
		// also handles browsers whose 'nearest' block alignment differs
		// subtly from Pierre's (we want the row visible, not pinned to the
		// edge).
		tryDirectScroll(local, path);

		if (!hadTabIndex) {
			scrollEl.removeAttribute('tabindex');
		}

		if (previousFocus && getDeepActiveElement() !== previousFocus) {
			previousFocus.focus({ preventScroll: true });
		}
	}

	function tryDirectScroll(local: FileTree, path: string): boolean {
		const root = local.getFileTreeContainer()?.shadowRoot;
		if (!root) {
			return false;
		}
		const escaped = typeof CSS !== 'undefined' && CSS.escape ? CSS.escape(path) : path.replace(/"/g, '\\"');
		// Match the flow row, not the sticky overlay clone (which also
		// carries `data-item-path` but lives at a fixed top offset).
		const row = root.querySelector<HTMLElement>(`[data-item-path="${escaped}"]:not([data-file-tree-sticky-row])`);
		if (!row) {
			return false;
		}
		row.scrollIntoView({ block: 'nearest' });
		return true;
	}

	function getDeepActiveElement(): HTMLElement | null {
		let active: Element | null = document.activeElement;
		while (active && active.shadowRoot && active.shadowRoot.activeElement) {
			active = active.shadowRoot.activeElement;
		}
		return active instanceof HTMLElement ? active : null;
	}

	// Pull DOM focus onto a tree row when WorkspaceState bumps the
	// sidebar focus tick (F6 cycle, Ctrl+0). Pierre's rows live inside
	// a Shadow DOM on the `<file-tree-container>` host, so a light-DOM
	// `querySelector` from Sidebar.svelte never finds them — only the
	// header button is reachable from there, which is exactly the
	// "Open folder" detour the user complained about.
	//
	// Strategy: ask Pierre to put logical focus on the closest visible
	// row to the active file (or the existing focused/first row when
	// none), wait for Svelte's microtask flush so Pierre's preact view
	// has stamped `tabindex=0` on it, then reach into the shadow root
	// and call DOM `focus()` on that button. That makes arrow keys
	// fire on the row directly, the way Pierre's keymap expects.
	$effect(() => {
		const focusTick = workspace.sidebarFocusTick;
		if (focusTick === 0) {
			return;
		}
		// Active path is read but must not be tracked: this effect's
		// only trigger is the focus tick. Without `untrack`, every tab
		// switch (which changes activePath) would yank focus into the
		// tree once the tick has been bumped at least once.
		const target = untrack(() => workspace.activePath);
		void pullFocusIntoTree(target);
	});

	async function pullFocusIntoTree(activePath: string | null) {
		const local = tree;
		if (!local) {
			return;
		}
		// `focusNearestPath` resolves the nearest *visible* path:
		//   - active path if expanded into view → exact match
		//   - active path inside a collapsed dir → nearest visible
		//     ancestor
		//   - null → existing focused row, or first row if none
		// In all cases the controller updates its focused index and
		// emits, which queues a preact re-render.
		local.focusNearestPath(activePath);
		// `await tick()` waits for Svelte's next microtask, by which
		// point preact (which schedules via `Promise.resolve().then`)
		// has flushed the new `tabindex=0` to the DOM. Belt-and-braces:
		// also try a raf if the first query misses (defensive against
		// version drift in Pierre's render pipeline).
		await tick();
		if (focusTreeRow(local)) {
			return;
		}
		await new Promise<void>((resolve) => requestAnimationFrame(() => resolve()));
		focusTreeRow(local);
	}

	function focusTreeRow(local: FileTree): boolean {
		const root = local.getFileTreeContainer()?.shadowRoot;
		if (!root) {
			return false;
		}
		const row =
			root.querySelector<HTMLElement>('[role="treeitem"][tabindex="0"]') ??
			root.querySelector<HTMLElement>('[role="treeitem"]');
		if (!row) {
			return false;
		}
		row.focus();
		return true;
	}

	// Promote the row the user is currently on (focused, possibly
	// different from the selected one when they've been arrow-key
	// navigating) into the editor with focus. Directories are left to
	// Pierre's existing ArrowRight/Left expansion bindings — adding
	// Enter-to-toggle would be a small follow-up but not what the
	// current request is about.
	function activateFocusedRow() {
		if (!tree) {
			return;
		}
		const focused = tree.getFocusedPath();
		if (!focused) {
			return;
		}
		const item = tree.getItem(focused);
		if (!item || item.isDirectory()) {
			return;
		}
		// Default `focus: true` re-issues the editor focus tick. If the
		// file isn't open yet (focused but not yet selected — possible
		// after pure arrow-key navigation, since Pierre only updates
		// selection on click) this opens it; if it is, we just bump
		// the focus ticker.
		void workspace.openFile(focused);
	}

	// Pierre stops propagation for keys it handles (arrows, Home/End,
	// Ctrl+A, Ctrl+Space, Esc/Enter inside search and renaming). Plain
	// Enter on a row falls through to us. We don't need to filter input
	// elements here for the same reason — Pierre's search/rename inputs
	// own their own Enter behaviour and never let it bubble.
	function onKeyDown(event: KeyboardEvent) {
		if (event.key !== 'Enter') {
			return;
		}
		event.preventDefault();
		activateFocusedRow();
	}

	function onDblClick() {
		activateFocusedRow();
	}
</script>

<!-- svelte-ignore a11y_no_static_element_interactions -->
<div class="tree" bind:this={mount} onkeydown={onKeyDown} ondblclick={onDblClick}></div>

<style>
	.tree {
		height: 100%;
		width: 100%;
		overflow: hidden;
		--trees-row-font-family: var(--m-font-ui);
	}
</style>
