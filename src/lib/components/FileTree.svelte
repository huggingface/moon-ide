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

	// Reactively reset paths when the workspace path list changes, then
	// replay the active path so Save As (which mutates `activePath`
	// *before* the new file lands in `paths`, with an `await` between
	// the two) doesn't end up with the new row unselected, the keyboard
	// cursor stuck on row 0, and the list scrolled to the top.
	//
	// We can't rely on the activePath effect to re-fire here — its only
	// dep is `activePath`, which didn't change. And we can't merge the
	// two effects, because `resetPaths` is expensive enough that running
	// it on every tab switch would be wasteful.
	$effect(() => {
		const paths = workspace.paths;
		if (!tree) {
			return;
		}
		tree.resetPaths(paths);
		// `untrack`: this effect is for `paths`. Without it, every
		// tab switch would wedge resetPaths between the activePath
		// change and its handler.
		const target = untrack(() => workspace.activePath);
		applySelection(tree, target, { afterReset: true });
	});

	// Mirror the active file in the tree's selection so the row stays
	// highlighted as the user switches tabs (or restores a session).
	$effect(() => {
		const target = workspace.activePath;
		if (!tree) {
			return;
		}
		applySelection(tree, target, { afterReset: false });
	});

	// Two invariants:
	//   1. If a file is active, exactly that file's row is selected
	//      and (when the row is virtualized) Pierre's focused index
	//      tracks it via the scroll fallback below.
	//   2. If no file is active, the selection is cleared — leaving a
	//      stale row selected makes re-clicking the same row a no-op
	//      (Pierre only fires `onSelectionChange` on real changes).
	//
	// `afterReset` widens the work we do: a tab switch can early-return
	// when selection already matches (avoids a feedback loop with the
	// click → onSelectionChange → activePath path), but a paths reset
	// must always re-scroll even if Pierre happened to preserve
	// selection by path string.
	//
	// We deliberately do **not** call `focusNearestPath(target)` here.
	// `scrollPathIntoView`'s fallback path needs `focusedPathChanged`
	// to be live when it focuses the shadow scroll container, so
	// Pierre's layout effect runs `scrollFocusedRowIntoView`. Calling
	// `focusNearestPath` up-front consumes the change with
	// `shouldOwnDomFocus=false` (focus still in the editor), and the
	// fallback's second call becomes a no-op — scroll never fires.
	// The fallback already updates the focused index for us.
	function applySelection(local: FileTree, target: string | null, opts: { afterReset: boolean }) {
		const current = local.getSelectedPaths();
		const alreadyInSync = target === null ? current.length === 0 : current.length === 1 && current[0] === target;
		if (alreadyInSync && !opts.afterReset) {
			return;
		}
		for (const sel of current) {
			if (sel !== target) {
				local.getItem(sel)?.deselect();
			}
		}
		if (target !== null) {
			// Expand collapsed ancestors first: select() works on a logical
			// path even when the row isn't rendered, but scroll-into-view
			// can only find DOM rows that actually exist. Without this,
			// opening `crates/moon-core/src/host.rs` from a Markdown link
			// (or after a session restore) leaves the file selected but
			// hidden inside a collapsed `crates/moon-core/` segment.
			expandAncestors(local, target);
			local.getItem(target)?.select();
			void scrollPathIntoView(local, target);
		}
	}

	// Walk the ancestor chain from the workspace root down to the file's
	// parent and call `expand()` on every directory handle that resolves.
	// `flattenEmptyDirectories: true` means some intermediate path strings
	// (`crates`, `crates/moon-core`) live as flattened segments without a
	// standalone row; `getItem` returns `null` for those, which we just
	// skip — expanding the deepest visible ancestor reveals the rest of
	// the chain because Pierre re-projects the flatten on each expand.
	function expandAncestors(local: FileTree, path: string) {
		const segments = path.split('/').filter(Boolean);
		if (segments.length <= 1) {
			return;
		}
		let cumulative = '';
		for (let i = 0; i < segments.length - 1; i++) {
			cumulative = cumulative ? `${cumulative}/${segments[i]}` : (segments[i] ?? '');
			const item = local.getItem(cumulative);
			// `'expand' in item` is the cleanest narrow from
			// `FileTreeItemHandle` to `FileTreeDirectoryHandle`:
			// `isDirectory()` returns `boolean` and doesn't act as a
			// type predicate, so TypeScript can't narrow off it on
			// its own.
			if (!item || !('expand' in item)) {
				continue;
			}
			if (!item.isExpanded()) {
				item.expand();
			}
		}
	}

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
		// Force `focusedPathChanged` to be true even if the controller
		// already happens to point at `path` (Pierre can preserve the
		// focused index across `resetPaths` when the path still exists,
		// in which case a single `focusNearestPath(path)` would dedupe
		// and skip the scroll).
		local.focusNearestPath(null);
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
	// Enter and Delete on a row fall through to us. We still defensively
	// bail out for Delete/Backspace when an `<input>` or `<textarea>`
	// holds focus inside the tree's shadow DOM (Pierre's search box and
	// future rename input), so typing inside those fields can never
	// trigger a delete confirm.
	//
	// `Delete` (and `Backspace` for macOS hardware that lacks a Delete
	// key) moves to the OS trash — reversible from the file manager.
	// `Shift+Delete` / `Shift+Backspace` skip the trash and remove the
	// path permanently; the confirm dialog wording differs accordingly.
	//
	// Targeting policy: act on the full multi-selection when there is
	// one (Pierre supports Ctrl+A, Shift+click, Ctrl+click ranges).
	// Fall back to the focused row when nothing is selected — Pierre
	// only updates selection on click, so a user who just arrowed onto
	// a row still gets to delete it without an extra Space first.
	function onKeyDown(event: KeyboardEvent) {
		if (event.key === 'Enter') {
			event.preventDefault();
			activateFocusedRow();
			return;
		}
		if (event.key === 'Delete' || event.key === 'Backspace') {
			if (isTextInputFocused()) {
				return;
			}
			if (!tree) {
				return;
			}
			const targets = collectRemovalTargets(tree);
			if (targets.length === 0) {
				return;
			}
			event.preventDefault();
			if (event.shiftKey) {
				void workspace.deletePaths(targets);
			} else {
				void workspace.trashPaths(targets);
			}
		}
	}

	// Pierre tracks selection (click-driven) and focus (arrow-key cursor)
	// independently. We act on the full selection only when the keyboard
	// cursor sits on a selected row — that's the multi-delete case
	// (Ctrl+click / Shift+click / Ctrl+A then Delete). When the cursor
	// has moved off the selection via arrow keys, fall back to the
	// focused row alone so Delete acts where the user thinks they are
	// rather than on the originally-clicked file.
	function collectRemovalTargets(local: FileTree): string[] {
		const focused = local.getFocusedPath();
		// Pierre returns `readonly string[]`; we hand the result on to
		// `WorkspaceState`, which mutates intermediate copies further
		// down — defensively clone here rather than sprinkle reads of
		// the readonly view across the call chain.
		const selected = [...local.getSelectedPaths()];
		if (focused && selected.includes(focused)) {
			return selected;
		}
		if (focused) {
			return [focused];
		}
		return selected;
	}

	function isTextInputFocused(): boolean {
		const active = getDeepActiveElement();
		if (!active) {
			return false;
		}
		if (active.isContentEditable) {
			return true;
		}
		const tag = active.tagName;
		return tag === 'INPUT' || tag === 'TEXTAREA';
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
