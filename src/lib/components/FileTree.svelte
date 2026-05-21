<script lang="ts">
	// Heads up: Svelte's compiler lexes this file's script body as
	// raw text looking for a closing script tag, and it will mistake
	// any stringified HTML tag — even in a code comment or template
	// literal — for one, bailing out with "script was left open".
	// If you need to reference an HTML element by name in prose,
	// write it out (e.g. "the style element") rather than wrapping
	// its tag name in angle brackets.
	import { mount as mountComponent, unmount, onMount, tick, untrack } from 'svelte';
	import {
		FileTree,
		type ContextMenuItem as PierreContextMenuItem,
		type ContextMenuOpenContext as PierreContextMenuOpenContext,
		type FileTreeBatchOperation,
		type FileTreeDirectoryHandle,
		type FileTreeMutationEvent,
		type FileTreeRenameEvent,
	} from '@pierre/trees';
	import ContextMenu from './ContextMenu.svelte';
	import type { ContextMenuItem } from './contextMenu';
	import { workspace } from '../state.svelte';
	import { ipc } from '../ipc';
	import { formatError, type GitFileStatus, type GitStatusEntry } from '../protocol';

	type Props = {
		/**
		 * `'all'` (default) renders the full workspace path list;
		 * `'changes'` renders only paths with a non-ignored git
		 * status entry, fully expanded, and clicks open in diff
		 * mode. Used by the SCM panel's changes-only filter — the
		 * sidebar mounts both modes simultaneously and toggles
		 * visibility, so each tree's expansion state survives
		 * round-trips through the toggle.
		 */
		mode?: 'all' | 'changes';
	};
	let { mode = 'all' }: Props = $props();

	let treeMount: HTMLDivElement;
	let tree: FileTree | undefined;

	// Set to `true` while `applySelection` is mirroring
	// `workspace.activePath` into Pierre's selection. Pierre fires
	// `onSelectionChange` synchronously from `.select()`, and without
	// this gate every tab switch would re-run the per-mode click
	// policy in `activateRowFromTree` — flipping a modified file into
	// diff mode just because the user clicked its tab while the
	// changes-mode tree was the visible sidebar. Real user clicks
	// reach `activateRowFromTree` through the wrapper-level
	// `onTreeClick` handler, which already covers every click
	// (including the already-selected-row case Pierre's selection
	// callback misses), so silencing the callback here doesn't lose
	// any genuine user gesture.
	let syncingSelection = false;

	// Tracks the Svelte component instance currently rendered inside
	// Pierre's context-menu slot so we can tear it down when Pierre
	// tells us to close (and when the menu is replaced with a new one
	// for a different row). Pierre removes the DOM node on close, but
	// the Svelte instance still holds reactive state + event wiring
	// and needs an explicit `unmount` to free it.
	let activeMenuInstance: ReturnType<typeof mountComponent> | null = null;
	// Portaled menu host. We render the menu into `document.body`
	// rather than into Pierre's slot because Pierre's scroll container
	// has `overflow: hidden` and clips a slotted popover — even when
	// the popover itself is `position: fixed`, the sticky-row ancestor
	// has `will-change: transform` which turns it into a containing
	// block for fixed descendants. Portaling sidesteps both problems.
	let activeMenuHost: HTMLDivElement | null = null;

	// Nudges we inject into Pierre's shadow DOM via `@layer unsafe`.
	// Kept deliberately tiny — every rule here is one we'd rather
	// upstream once Pierre exposes a cleaner hook.
	//
	// 1. Pierre's default 6px flex gap plus the 12px git lane puts
	//    the git dot uncomfortably close to the hover ellipsis; the
	//    extra right-margin on the git cell restores ~10px of visual
	//    breathing room without touching the global row gap.
	// Read the warning token from `--m-warning` defined on `:root`,
	// resolved at module-import time. Pierre lives in a closed
	// shadow root that doesn't inherit our custom properties, so
	// the colour has to be baked into the injected stylesheet
	// literally — referencing `var(--m-warning)` from inside the
	// shadow would resolve to the unset default and render the
	// badge invisible. Reading from `getComputedStyle(document.
	// documentElement)` would mean re-running on theme flips; the
	// IDE's only theme flip today is a hard reload, so the
	// build-time read is enough.
	const CONFLICT_BADGE_COLOR =
		(typeof document !== 'undefined'
			? getComputedStyle(document.documentElement).getPropertyValue('--m-warning').trim()
			: '') || '#f0b86e';

	const PIERRE_OVERRIDES_CSS = `
[data-item-section='git'] {
	margin-right: 8px;
}
/* Conflict-marker badge. The Pierre row decoration cell holds
   our "!" text; we tint it with the IDE's warning token (baked
   in at module load — see CONFLICT_BADGE_COLOR) and bump it to
   bold so a single "!" on a busy row still catches the eye. */
[data-item-section='decoration'] {
	color: ${CONFLICT_BADGE_COLOR};
	font-weight: 700;
}
`;

	// Pierre's `gitStatus` option accepts a fixed vocabulary
	// (`added | deleted | ignored | modified | renamed | untracked`).
	// Our own enum extends it with `conflicted`, which Pierre would
	// type-reject. Translate to the closest Pierre token (`modified`
	// — a conflicted file *is* modified vs. HEAD on the non-conflict
	// side) and overlay the actual conflict signal via
	// `renderRowDecoration` below, so the row gets both the regular
	// status colour and an unmistakable badge.
	function entriesForPierre(entries: readonly GitStatusEntry[]) {
		return entries.map((entry) => ({
			path: entry.path,
			status: entry.status === 'conflicted' ? ('modified' as const) : entry.status,
		}));
	}

	// Per-file membership in `gitStatusEntries === 'conflicted'`,
	// rebuilt on every status refresh. The `renderRowDecoration`
	// callback Pierre invokes per visible row reads this set
	// directly — building a `Set<string>` keeps the lookup O(1)
	// even on repos with thousands of changed paths.
	let conflictedPaths: ReadonlySet<string> = new Set();
	function recomputeConflictedPaths(entries: readonly GitStatusEntry[]) {
		const next = new Set<string>();
		for (const entry of entries) {
			if (entry.status === 'conflicted') {
				next.add(entry.path);
			}
		}
		conflictedPaths = next;
	}

	onMount(() => {
		if (!treeMount) {
			return;
		}
		recomputeConflictedPaths(untrack(() => workspace.gitStatusEntries));
		tree = new FileTree({
			paths: currentPaths(),
			flattenEmptyDirectories: true,
			unsafeCSS: PIERRE_OVERRIDES_CSS,
			// `'all'` mode starts every folder collapsed (the old
			// default; an `initialExpansion: 1` would eagerly open
			// gitignored folders like `node_modules/` / `target/`
			// before we knew they were ignored). `'changes'` mode
			// is naturally short — only changed paths — so we
			// expand fully so the user sees every change without
			// having to drill in.
			initialExpansion: mode === 'changes' ? 'open' : 0,
			search: true,
			gitStatus: entriesForPierre(untrack(() => workspace.gitStatusEntries)),
			// Per-row decoration for unmerged paths. Pierre's
			// own `GitStatus` doesn't carry a `conflicted`
			// token, so we map those rows to `modified` for the
			// regular status colour (see `entriesForPierre`) and
			// stamp a small "!" badge here on top. The badge sits
			// in Pierre's dedicated decoration cell — it doesn't
			// clash with the rename inline edit or the context-
			// menu hover button.
			renderRowDecoration: ({ item }) => {
				if (item.kind !== 'file') {
					return null;
				}
				if (!conflictedPaths.has(item.path)) {
					return null;
				}
				return {
					text: '!',
					title: 'Unresolved merge conflict — edit the file to resolve, then save.',
				};
			},
			onSelectionChange: (selectedPaths) => {
				if (selectedPaths.length === 0) {
					return;
				}
				const path = selectedPaths[0];
				if (path === undefined) {
					return;
				}
				// Skip programmatic selection mirrors (see
				// `syncingSelection` above). Real user clicks still
				// reach `activateRowFromTree` via `onTreeClick`.
				if (syncingSelection) {
					return;
				}
				// Both modes stay mounted (CSS-toggled visibility) so
				// Pierre's `select()` calls produce
				// `onSelectionChange` events on the *hidden* tree
				// too. Without this gate the hidden changes-view
				// tree would fire its own activate (with
				// `mode: 'changes'`) on every active-path mirror,
				// silently flipping diff mode on for files the user
				// only ever clicked in the regular all-view.
				if (!isVisibleMode()) {
					return;
				}
				activateRowFromTree(path);
			},
			renaming: {
				onRename: handleRename,
				onError: (message) => {
					workspace.flash(message);
				},
			},
			composition: {
				contextMenu: {
					// Both triggers so mouse users get the right-click
					// path they expect from a file tree and the hover
					// ellipsis button covers keyboard / trackpad users
					// who'd rather click than two-finger-tap. Button
					// visibility defaults to `when-needed` — the
					// ellipsis only appears on the hovered / focused
					// row, keeping the tree visually quiet at rest.
					enabled: true,
					triggerMode: 'both',
					buttonVisibility: 'when-needed',
					render: renderRowContextMenu,
					onClose: () => {
						disposeActiveMenu();
					},
				},
			},
		});
		// Mirror Pierre's own mutations into `lastTreePaths` so the
		// reactive diff effect doesn't try to re-add a placeholder
		// the user just confirmed (or re-remove one they cancelled).
		// Pierre fires these for every internal change — our
		// `tree.batch` calls included — so the bookkeeping is
		// uniform regardless of who triggered the mutation.
		tree.onMutation('*', mirrorMutation);
		tree.render({ containerWrapper: treeMount });
		applyGitOverlay(
			tree,
			untrack(() => workspace.gitStatusEntries),
		);
		return () => {
			disposeActiveMenu();
			tree?.cleanUp();
			tree = undefined;
			lastTreePaths = null;
			pendingNewKind.clear();
		};
	});

	// Build the menu DOM for Pierre. Two elements involved: a zero-
	// sized **anchor** we hand back so Pierre's own bookkeeping
	// (slot population, focus-restore target, re-open toggling) stays
	// happy, and a **portaled host** in `document.body` where the
	// real popover lives. The popover can't render inside Pierre's
	// shadow DOM: the sticky-row wrapper has `will-change: transform`
	// (which makes it the containing block for our `position: fixed`
	// popover) and the scroll container has `overflow: hidden`, so
	// either clips the menu. `data-file-tree-context-menu-root` goes
	// on both so Pierre's outside-click detection treats clicks
	// inside the portaled menu as "inside the menu surface".
	//
	// Returning `null` skips the menu altogether for rows with no
	// applicable actions (e.g. a clean row in a non-git folder).
	function renderRowContextMenu(
		item: PierreContextMenuItem,
		context: PierreContextMenuOpenContext,
	): HTMLElement | null {
		const items = buildMenuItems(item);
		if (items.length === 0) {
			return null;
		}
		// A second `render` call replaces the first. Tear down the
		// previous instance before mounting the new one so we don't
		// leak Svelte components or orphan the portaled host.
		disposeActiveMenu();

		const anchor = document.createElement('div');
		anchor.setAttribute('data-file-tree-context-menu-root', 'true');

		const host = document.createElement('div');
		host.setAttribute('data-file-tree-context-menu-root', 'true');
		// Zero-size portaled container. The popover inside uses
		// `position: fixed` and positions itself against the viewport,
		// so the host doesn't need any layout of its own — making it
		// 0×0 guarantees it never intercepts pointer events outside
		// the popover's own bounding box. `position: fixed` here is
		// only so an unstyled child-without-fixed wouldn't end up
		// anchored to the document flow.
		host.style.position = 'fixed';
		host.style.top = '0';
		host.style.left = '0';
		host.style.width = '0';
		host.style.height = '0';
		host.style.zIndex = '9999';
		document.body.appendChild(host);

		const instance = mountComponent(ContextMenu, {
			target: host,
			props: {
				items,
				anchorRect: context.anchorRect,
				onClose: () => {
					context.close();
				},
			},
		});
		activeMenuInstance = instance;
		activeMenuHost = host;
		return anchor;
	}

	function disposeActiveMenu() {
		if (activeMenuInstance) {
			void unmount(activeMenuInstance);
			activeMenuInstance = null;
		}
		if (activeMenuHost) {
			activeMenuHost.remove();
			activeMenuHost = null;
		}
	}

	// Map a row + its backend git status to menu items. Order is
	// chosen so common-case actions (New File / New Folder, the
	// reason most users right-click) sit at the top, with destructive
	// or status-conditional items below. The menu intentionally
	// stays small — every line is one the user has to scan.
	function buildMenuItems(item: PierreContextMenuItem): ContextMenuItem[] {
		const items: ContextMenuItem[] = [];
		// "New File" and "New Folder" always appear, on file rows
		// and folder rows alike. VSCode/Cursor convention: a
		// right-click on a file creates the new entry as a sibling
		// in the same parent directory; a right-click on a folder
		// creates it inside that folder. `targetDirectoryFor` does
		// the resolution so the menu callback can call into the
		// inline-rename flow with the right anchor.
		const targetDir = targetDirectoryFor(item);
		items.push({
			id: 'new-file',
			label: 'New file',
			onSelect: () => {
				void startNewItem(targetDir, 'file');
			},
		});
		items.push({
			id: 'new-folder',
			label: 'New folder',
			onSelect: () => {
				void startNewItem(targetDir, 'folder');
			},
		});
		// Rename uses Pierre's built-in inline-rename flow; the row
		// becomes an editable input, Enter commits, Esc cancels.
		// The workspace root itself can't be renamed via the tree
		// (it's a folder bar, not a path inside the tree), but every
		// other row qualifies.
		items.push({
			id: 'rename',
			label: 'Rename',
			onSelect: () => {
				if (!tree) {
					return;
				}
				tree.startRenaming(item.path);
			},
		});
		if (item.kind === 'file' && canViewDiff(item.path)) {
			items.push({
				id: 'view-diff',
				label: 'View diff',
				onSelect: () => {
					// Single-tab + mode toggle: flipping the diff
					// flag and opening the file lands the user in
					// the diff view of the same buffer the editor
					// otherwise renders. The toggle is also exposed
					// as a tab button (Source / Diff) and a
					// keybind (Ctrl/Cmd-Shift-D).
					workspace.setDiffMode(item.path, true);
					void workspace.openFile(item.path);
				},
			});
		}
		const discardPaths = collectDiscardPaths(item);
		if (discardPaths.length > 0) {
			items.push({
				id: 'discard',
				label: discardLabel(item, discardPaths),
				kind: 'danger',
				onSelect: () => {
					void workspace.discardPaths(discardPaths);
				},
			});
		}
		items.push({
			id: 'copy-path',
			label: 'Copy path',
			onSelect: () => {
				void navigator.clipboard?.writeText(item.path).catch(() => {
					// Clipboard can reject when the window isn't
					// focused during the prompt; the close already
					// fired, so all we can do is drop the action.
					// Not a user-facing error — any reason this
					// would fail is a browser-policy boundary.
				});
			},
		});
		return items;
	}

	/**
	 * Where a "New file" / "New folder" gesture should anchor when
	 * the user right-clicked `item`. Folder rows yield themselves
	 * (creating inside that folder); file rows yield their parent
	 * directory (creating as a sibling). `''` means "create at the
	 * workspace root" — Pierre's empty path string, by convention.
	 */
	function targetDirectoryFor(item: PierreContextMenuItem): string {
		if (item.kind === 'directory') {
			// Pierre's directory paths carry a trailing slash; we
			// keep it because both `tree.add` and our own backend
			// walk treat `foo/` as the canonical directory id.
			return item.path;
		}
		const lastSlash = item.path.lastIndexOf('/');
		if (lastSlash < 0) {
			// Top-level file → create new sibling at the root.
			return '';
		}
		return item.path.slice(0, lastSlash + 1);
	}

	/**
	 * Kick off Pierre's inline-rename flow for a not-yet-existing
	 * file or folder under `targetDir`. The placeholder gets a
	 * unique synthetic name, gets added to the tree, gets recorded
	 * in `pendingNewKind` so `handleRename` knows the upcoming
	 * commit is a creation (not a rename of an existing path), and
	 * gets handed to `startRenaming`. `removeIfCanceled` makes the
	 * Esc-cancel path throw the placeholder away cleanly; our
	 * `mirrorMutation` listener picks up the implicit `remove` and
	 * drops the pending entry.
	 */
	async function startNewItem(targetDir: string, kind: 'file' | 'folder') {
		if (!tree) {
			return;
		}
		// Make sure the parent is expanded so the inline input is
		// visible. Without this, "new file inside `src/`" while
		// `src/` is collapsed silently puts the input off-screen.
		if (targetDir !== '') {
			const parent = tree.getItem(targetDir);
			if (parent && 'expand' in parent && !parent.isExpanded()) {
				parent.expand();
			}
		}
		const placeholder = uniquePlaceholder(targetDir, kind);
		pendingNewKind.set(placeholder, kind);
		tree.add(placeholder);
		await tick();
		const ok = tree.startRenaming(placeholder, { removeIfCanceled: true });
		if (!ok) {
			// Pierre refused (rare — the only reason it returns
			// false today is the path not being in the tree, which
			// we just added). Drop the bookkeeping; the placeholder
			// is harmless on its own.
			pendingNewKind.delete(placeholder);
		}
	}

	/**
	 * Build a placeholder path under `targetDir` that doesn't
	 * collide with any existing tree entry. We use a `~moon-new-…`
	 * prefix so the synthetic row is obviously transient if the
	 * UI ever surfaces it (it shouldn't — `startRenaming` swaps
	 * the row for an inline input immediately).
	 */
	function uniquePlaceholder(targetDir: string, kind: 'file' | 'folder'): string {
		const suffix = kind === 'folder' ? '/' : '';
		const base = `${targetDir}~moon-new`;
		let candidate = `${base}${suffix}`;
		let counter = 2;
		while (tree?.getItem(candidate) !== null && tree?.getItem(candidate) !== undefined) {
			candidate = `${base}-${counter}${suffix}`;
			counter += 1;
		}
		return candidate;
	}

	/**
	 * Called when Pierre completes an inline rename (Enter on the
	 * input). Two cases share this entry point:
	 *
	 * 1. The row was a real, on-disk path: this is a true rename.
	 *    Dispatch to `workspace.renamePath` which handles the
	 *    backend `fs_rename`, fixes up open buffers, and triggers
	 *    a tree refresh.
	 *
	 * 2. The row was a placeholder we added via `startNewItem`:
	 *    `pendingNewKind` carries the entry. Dispatch to
	 *    `workspace.createFile` / `workspace.createDir` instead;
	 *    the placeholder is purely client-side and was never on
	 *    disk, so there's nothing to rename.
	 *
	 * In both cases Pierre has already moved the row in its store
	 * by the time we land here (the move is synchronous within
	 * `commit`); our backend call just needs to make disk match.
	 */
	function handleRename(event: FileTreeRenameEvent) {
		const pending = pendingNewKind.get(event.sourcePath);
		if (pending !== undefined) {
			pendingNewKind.delete(event.sourcePath);
			// The destination Pierre handed us carries a trailing
			// slash for folders, no slash for files — exactly what
			// our backend expects. Strip the placeholder from
			// `pendingNewKind` first so a follow-up failure toast
			// doesn't leave it lying around.
			if (pending === 'file') {
				const cleanDest = event.destinationPath.endsWith('/')
					? event.destinationPath.slice(0, -1)
					: event.destinationPath;
				void workspace.createFile(cleanDest);
			} else {
				const cleanDest = event.destinationPath.endsWith('/') ? event.destinationPath : `${event.destinationPath}/`;
				void workspace.createDir(cleanDest);
			}
			return;
		}
		void workspace.renamePath(event.sourcePath, event.destinationPath);
	}

	/**
	 * Tracks paths the user is in the middle of creating. Maps the
	 * synthetic placeholder we hand to `tree.add` → the kind they
	 * picked from the menu, so `handleRename` can route the commit
	 * to `createFile` vs `createDir`. Cleared on commit, on cancel
	 * (via `mirrorMutation`'s `remove` branch), and on tree teardown.
	 */
	const pendingNewKind = new Map<string, 'file' | 'folder'>();

	/**
	 * Mirror every Pierre-side mutation into `lastTreePaths`. The
	 * reactive diff effect needs `lastTreePaths` to reflect Pierre's
	 * actual current path-set, not just the post-batch result of
	 * our own writes — when the user kicks off a New File flow,
	 * Pierre adds a placeholder, then renames it, then either
	 * commits (we IPC-create the file and the next watcher tick
	 * lands the same path in `workspace.paths`) or cancels (Pierre
	 * removes the placeholder). Without mirroring, the diff effect
	 * would see `workspace.paths` not contain the placeholder and
	 * try to remove a path Pierre already moved or removed — a
	 * batch error. Listening on `onMutation('*')` keeps the mirror
	 * consistent regardless of which method (`add`, `move`,
	 * `remove`, even our own `batch`) caused the change.
	 */
	function mirrorMutation(event: FileTreeMutationEvent) {
		if (lastTreePaths === null) {
			return;
		}
		const next = new Set(lastTreePaths);
		applyMutationToSet(event, next);
		lastTreePaths = next;
	}

	function applyMutationToSet(event: FileTreeMutationEvent, set: Set<string>) {
		switch (event.operation) {
			case 'add':
				set.add(event.path);
				break;
			case 'remove':
				set.delete(event.path);
				if (event.recursive && event.path.endsWith('/')) {
					// Pierre dropped every descendant in one shot; mirror that.
					// We collect-then-delete instead of iterating-and-mutating
					// so the iterator's invalidation invariants don't bite.
					const descendants: string[] = [];
					for (const p of set) {
						if (p.startsWith(event.path)) {
							descendants.push(p);
						}
					}
					for (const p of descendants) {
						set.delete(p);
					}
				}
				// Cancellation cleanup: the placeholder we tracked
				// for a not-yet-committed New File / New Folder is
				// gone now, drop the bookkeeping.
				pendingNewKind.delete(event.path);
				break;
			case 'move': {
				set.delete(event.from);
				set.add(event.to);
				const fromPrefix = event.from.endsWith('/') ? event.from : `${event.from}/`;
				const toPrefix = event.to.endsWith('/') ? event.to : `${event.to}/`;
				// Same collect-then-mutate pattern as the recursive
				// remove branch above — rewriting descendant paths
				// in place would invalidate the set's iterator.
				const moved: { from: string; to: string }[] = [];
				for (const p of set) {
					if (p.startsWith(fromPrefix)) {
						moved.push({ from: p, to: toPrefix + p.slice(fromPrefix.length) });
					}
				}
				for (const m of moved) {
					set.delete(m.from);
					set.add(m.to);
				}
				break;
			}
			case 'reset':
				// Wholesale reset wipes every path; the diff effect
				// itself ran through our code path with a fresh set
				// it'll write back next, so we just clear here and
				// let `lastTreePaths` get rewritten on the next
				// batch / reset call.
				set.clear();
				break;
			case 'batch':
				for (const sub of event.events) {
					applyMutationToSet(sub, set);
				}
				break;
		}
	}

	/**
	 * True when "View diff" makes sense for `path`. We show it for
	 * modified files (HEAD vs working tree is a real diff) and for
	 * deleted files (HEAD vs empty — the shape a deletion takes in
	 * the diff view). Added / untracked / ignored rows have no
	 * `HEAD` side worth rendering and are excluded: git's own story
	 * for them is "new file from nothing", which is better served
	 * by just opening the file in the editor.
	 */
	function canViewDiff(path: string): boolean {
		const entry = workspace.gitStatusEntries.find((e) => e.path === path);
		if (!entry) {
			return false;
		}
		return entry.status === 'modified' || entry.status === 'deleted';
	}

	/**
	 * Paths that `workspace.discardPaths` should see when the user
	 * picks "Discard" on `item`. Files resolve to the file itself
	 * (if its own git status is discardable); folders fan out into
	 * every non-ignored, non-added change under them. 'added'
	 * descendants are skipped for the same reason the file-level
	 * action omits them — reverting staged-new is ambiguous between
	 * "unstage" and "delete" and we don't want a folder-scoped click
	 * to silently pick one. Ignored descendants are skipped because
	 * they aren't meaningfully a "change". An empty list means the
	 * menu should not offer a discard entry at all.
	 */
	function collectDiscardPaths(item: PierreContextMenuItem): string[] {
		const entries = workspace.gitStatusEntries;
		if (item.kind === 'file') {
			for (const entry of entries) {
				if (entry.path !== item.path) {
					continue;
				}
				if (isDiscardable(entry.status)) {
					return [entry.path];
				}
				return [];
			}
			return [];
		}
		// Directory: match the folder row itself (e.g. a wholly-
		// untracked `foo/`) plus every descendant. Pierre's
		// `data-item-path` for directories carries a trailing slash
		// which matches the backend's output, so `startsWith`
		// unambiguously means "strictly under this folder".
		const out: string[] = [];
		for (const entry of entries) {
			const isMatch = entry.path === item.path || entry.path.startsWith(item.path);
			if (!isMatch) {
				continue;
			}
			if (isDiscardable(entry.status)) {
				out.push(entry.path);
			}
		}
		return out;
	}

	function isDiscardable(status: GitFileStatus): boolean {
		return status === 'modified' || status === 'deleted' || status === 'untracked';
	}

	function discardLabel(item: PierreContextMenuItem, paths: readonly string[]): string {
		if (item.kind === 'file') {
			// Single-path path: use the exact status to tailor copy.
			// An untracked row is more honest as "move to trash" than
			// "discard" since git has nothing to revert — the label
			// tracks that. A deleted row recreates the file from
			// HEAD, which reads as "Restore" not "Discard".
			const entry = workspace.gitStatusEntries.find((e) => e.path === item.path);
			if (entry?.status === 'untracked') {
				return 'Discard (move untracked file to trash)';
			}
			if (entry?.status === 'deleted') {
				return 'Restore file';
			}
			return 'Discard changes';
		}
		// Directory: a count clarifies the scope so the user can
		// predict whether the confirm dialog is going to list one or
		// fifty paths. Singular/plural is worth the branch.
		if (paths.length === 1) {
			return 'Discard 1 change in this folder';
		}
		return `Discard ${paths.length} changes in this folder`;
	}

	// Feed git status into Pierre whenever the backend classifier
	// returns a new list. We pass ignored entries through too so
	// Pierre's native row styling kicks in — its
	// `[data-item-git-status='ignored']` selector colours the icon,
	// filename, and git lane in one consistent stroke that we'd
	// otherwise have to recreate by hand (and miss bits of, like the
	// content text colour). The trade-off is that Pierre's
	// `directoriesWithChanges` pass adds `data-item-contains-git-
	// change` to ancestors of ignored entries, so a folder like
	// `front/` lights up with a dot just because `front/node_modules/`
	// is ignored. `applyGitOverlay` below hides that dot for
	// ignored-only ancestors and tints the rest by the worst tracked
	// descendant status.
	$effect(() => {
		const entries = workspace.gitStatusEntries;
		if (!tree) {
			return;
		}
		recomputeConflictedPaths(entries);
		tree.setGitStatus(entriesForPierre(entries));
		applyGitOverlay(tree, entries);
	});

	type TrackedStatus = 'added' | 'modified' | 'deleted' | 'untracked';

	// Severity of a subtree's "worst" change. Mirrors Pierre's own
	// ordering of status tokens so the dot color on a folder matches
	// what the user would see if they opened the folder and scanned
	// the rows.
	const SEVERITY: Record<TrackedStatus, number> = {
		deleted: 4,
		modified: 3,
		added: 2,
		untracked: 1,
	};

	// Recompute the shadow-DOM overlay for folder dots. Runs on every
	// git-status refresh; the injected stylesheet is re-used so
	// subsequent updates are a single textContent swap.
	function applyGitOverlay(local: FileTree, entries: readonly GitStatusEntry[]) {
		const host = local.getFileTreeContainer();
		const shadow = host?.shadowRoot;
		if (!shadow) {
			return;
		}
		const css = buildOverlayCss(entries);
		let el = shadow.querySelector('style[data-moon-git-overlay]') as HTMLStyleElement | null;
		if (!el) {
			el = document.createElement('style');
			el.setAttribute('data-moon-git-overlay', '');
			shadow.appendChild(el);
		}
		if (el.textContent !== css) {
			el.textContent = css;
		}
	}

	function buildOverlayCss(entries: readonly GitStatusEntry[]): string {
		// Pierre materializes directory paths with a trailing slash
		// (`materializeNodePath` in `path-store/src/canonical.ts`
		// appends `/` for every directory node), so `data-item-path`
		// reads `target/` for folder rows and `target/foo.rs` for
		// files. Our selectors have to match that verbatim — earlier
		// iterations stripped the slash and silently missed the
		// folder row itself (plus every ancestor dot).
		//
		// We track two ancestor sets:
		//   - `folderSeverity` → folders with at least one tracked
		//     (added/modified/deleted/untracked) descendant, mapped
		//     to that descendant's worst status. These get a
		//     coloured dot tinted by that status.
		//   - `ignoredAncestors` → folders with at least one ignored
		//     descendant. Pierre still flags them with
		//     `data-item-contains-git-change="true"` and would render
		//     a modified-coloured dot by default, but a
		//     "node_modules/ contains 1000 ignored files" dot has no
		//     actionable signal — pure visual noise. We hide the dot
		//     when a folder appears here but not in
		//     `folderSeverity`.
		const folderSeverity = new Map<string, TrackedStatus>();
		const ignoredAncestors = new Set<string>();
		for (const entry of entries) {
			if (entry.path.length === 0) {
				continue;
			}
			// Conflicted entries collapse to `modified` for the
			// purpose of folder-dot severity — the row's badge
			// already signals the conflict; the ancestor dot
			// just needs the worst tracked colour, and a
			// conflict is at least as "tracked-modified" as a
			// regular modify.
			const trackedStatus: TrackedStatus | 'ignored' = entry.status === 'conflicted' ? 'modified' : entry.status;
			const stripped = entry.path.replace(/\/+$/, '');
			const segments = stripped.split('/');
			let cumulative = '';
			for (let i = 0; i < segments.length - 1; i++) {
				const seg = segments[i] ?? '';
				cumulative = cumulative === '' ? seg : `${cumulative}/${seg}`;
				const key = `${cumulative}/`;
				if (trackedStatus === 'ignored') {
					ignoredAncestors.add(key);
					continue;
				}
				const existing = folderSeverity.get(key);
				if (existing === undefined || SEVERITY[trackedStatus] > SEVERITY[existing]) {
					folderSeverity.set(key, trackedStatus);
				}
			}
		}

		const rules: string[] = [];

		// Hide the descendant-change dot on ancestors whose only
		// descendants are ignored. `visibility: hidden` keeps the
		// 12px lane reserved so filenames don't shift around as the
		// rule toggles on/off across refreshes. Pierre never renders
		// a label inside `[data-item-section='git']` for folders
		// (only files with a tracked status get a letter), so
		// hiding the whole section is equivalent to hiding the dot.
		const ignoredOnly: string[] = [];
		for (const folder of ignoredAncestors) {
			if (!folderSeverity.has(folder)) {
				ignoredOnly.push(folder);
			}
		}
		if (ignoredOnly.length > 0) {
			const selector = ignoredOnly
				.map(
					(f) =>
						`[data-item-path="${escapeAttr(f)}"][data-item-contains-git-change="true"] > [data-item-section="git"]`,
				)
				.join(', ');
			rules.push(`${selector} {\n\tvisibility: hidden;\n}`);
		}

		// One rule per status bucket so we emit at most four rules
		// for the per-folder dot color. Unlayered rules beat Pierre's
		// `@layer base` default — which hard-codes the modified color
		// — because unlayered rules always win over layered ones at
		// equal specificity.
		const buckets = new Map<TrackedStatus, string[]>();
		for (const [folder, status] of folderSeverity) {
			const list = buckets.get(status) ?? [];
			list.push(folder);
			buckets.set(status, list);
		}
		for (const [status, folders] of buckets) {
			const selector = folders
				.map(
					(f) =>
						`[data-item-path="${escapeAttr(f)}"][data-item-contains-git-change="true"] > [data-item-section="git"]`,
				)
				.join(', ');
			rules.push(`${selector} {\n\tcolor: var(--trees-git-${status}-color);\n}`);
		}

		return rules.join('\n\n');
	}

	// Escape a string for use inside a CSS attribute value in double
	// quotes. `CSS.escape` is for identifiers (too aggressive for
	// attribute values — it would escape `/` and `.` which are legal
	// inside `"…"`), so just neutralise the two characters that would
	// actually break out of the quoted literal.
	function escapeAttr(value: string): string {
		return value.replace(/\\/g, '\\\\').replace(/"/g, '\\"');
	}

	// Deleted files have no filesystem entry — they only exist in
	// git's view of the world — so `collectPaths` on the backend
	// can't surface them. Union them in so Pierre renders ghost rows
	// at the deleted path; its `deleted` status styling handles the
	// strikethrough. Added/modified/untracked paths come from the
	// filesystem walk already, so they don't need to be merged here.
	function mergedPathsWithDeletions(paths: readonly string[], entries: readonly GitStatusEntry[]): string[] {
		const present = new Set(paths);
		const out = paths.slice();
		for (const entry of entries) {
			if (entry.status !== 'deleted') {
				continue;
			}
			if (present.has(entry.path)) {
				continue;
			}
			present.add(entry.path);
			out.push(entry.path);
		}
		return out;
	}

	// `resetPaths` is expensive (Pierre rebuilds its virtualised
	// path model and drops every internal flag — expansion state,
	// focused row, selection memory). We gate it to two cases:
	//
	//   1. First paint after mount (no previous path set).
	//   2. The catastrophic-diff fallback when an incremental batch
	//      throws (Pierre's path-store invariant violation we
	//      didn't anticipate — should never happen with well-formed
	//      diffs, but better to recover than to leave a half-applied
	//      tree).
	//
	// Every other refresh diffs `lastTreePaths` against the new path
	// list and applies the result via `tree.batch(...)`. Pierre keeps
	// every other path's expansion / focus / selection memory intact
	// — deleting one file no longer collapses every expanded folder.
	//
	// Driven by the same trigger set as before: the filesystem path
	// list, plus the set of deleted ghost rows from git status.
	// `setGitStatus` handles status-only refreshes in place; this
	// effect only fires when the actual set of paths to render
	// changes.
	const deletedSignature = $derived(deletedPathsSignature(workspace.gitStatusEntries));
	const ignoredSignature = $derived(ignoredPathsSignature(workspace.gitStatusEntries));
	let lastTreePaths: ReadonlySet<string> | null = null;
	// Tracks the active folder seen by the last path-set effect run.
	// Switching folders bounces this and forces the wholesale
	// `resetPaths` branch — see the comment above the `resetPaths`
	// call below for the cost model.
	let lastEffectFolderPath: string | null = null;

	// Lazy-load bookkeeping for gitignored directories
	// (`node_modules/`, `target/`, …). The backend's
	// `fs_collect_paths` collapses them to a single trailing-slash
	// entry. We surface them in the tree but only walk into them
	// on demand: when the user expands one, we fetch its direct
	// children and add them via `tree.batch`. Children that are
	// themselves directories get added to `lazyDirs` so a deeper
	// expansion fires another fetch.
	//
	//  - `lazyDirs`: paths Pierre knows about but whose descendants
	//    haven't been walked yet. Click-or-keypress on one of these
	//    triggers a load.
	//  - `lazyLoading`: in-flight set to debounce repeated
	//    expansion events while a fetch is mid-IPC.
	//  - `lazyLoaded`: every path we've appended via the lazy
	//    flow. Re-unioned into `merged` so the next
	//    `gitStatusEntries`-driven path-set effect doesn't
	//    `applyPathsDiff` them away (the backend walk still
	//    excludes them).
	let lazyDirs = new Set<string>();
	let lazyLoading = new Set<string>();
	let lazyLoaded = new Set<string>();
	// Active folder seen by the last `seedLazyDirs` run. Mirrors
	// `lastEffectFolderPath` but lives on the lazy-load side so the
	// re-seed doesn't depend on effect execution order.
	let lazySeedFolderPath: string | null = null;

	$effect(() => {
		// Both modes ultimately re-derive paths from workspace
		// state, but the source signal differs. `'all'` keys off
		// the tree's filesystem walk plus the deleted-rows merge;
		// `'changes'` keys off `gitStatusEntries` directly (the
		// `scmFilterPaths` accessor folds in deleted entries and
		// strips ignored ones). Reading the right signal is what
		// makes Svelte 5 re-run the effect on the right input.
		let merged: readonly string[];
		if (mode === 'changes') {
			void workspace.gitStatusEntries;
			void deletedSignature;
			merged = workspace.scmFilterPaths;
		} else {
			const paths = workspace.paths;
			// Track signatures so an ignored or deleted entry
			// appearing / disappearing re-runs this effect. Add /
			// modify / untracked status flips produce the same
			// signature for both, so the noisy git-status refresh
			// stream doesn't re-fire the path-set work just to
			// flip a `Modified` flag — only structural changes
			// the tree cares about (deletions add ghost rows,
			// ignored dirs become lazy frontiers) do. We then
			// `untrack` the read of `gitStatusEntries` itself: the
			// signatures already captured "what changed", so
			// re-tracking the array would double-fire on every
			// refresh.
			void deletedSignature;
			void ignoredSignature;
			const entries = untrack(() => workspace.gitStatusEntries);
			merged = mergedPathsWithDeletions(paths, entries);
			// Re-seed `lazyDirs` whenever the active folder swap or
			// a fresh git-status refresh tells us the set of
			// collapsed-ignored directories, or the depth-capped
			// frontier the walk left for us. Same-folder refreshes
			// keep `lazyLoaded` so previously-expanded subtrees
			// stay populated. Only runs in 'all' mode — changes
			// mode never walks into ignored dirs by design.
			const depthCapped = workspace.depthCappedPaths;
			seedLazyDirs(workspace.activeFolderPath, entries, depthCapped);
			if (lazyLoaded.size > 0) {
				merged = mergeLazyLoaded(merged);
			}
		}
		if (!tree) {
			return;
		}
		// Skip Pierre work when this tree mode isn't visible. Both
		// `'all'` and `'changes'` trees stay mounted at all times
		// (CSS-toggled via the `.hidden` class on their wrapper),
		// so without this gate a folder switch pays the full
		// `resetPaths` / `applyPathsDiff` cost *twice* — once for
		// the visible tree the user sees, once for the hidden tree
		// they won't look at until they toggle the SCM filter. The
		// hidden tree's preact reconciliation still re-renders 30+
		// virtualised rows in its shadow DOM, triggers style
		// invalidation up to the host, and shows up downstream as
		// a multi-hundred-ms `recalculate-styles` event.
		//
		// To keep the catch-up cheap when the mode becomes visible
		// later, we bounce `lastTreePaths` to `null`. The next run
		// hits the `wholesaleFill`-equivalent path and does a one-
		// shot `resetPaths(merged)`. The user toggling SCM filter
		// is a deliberate gesture, not a hot path, so the
		// transition cost is fine to land there instead of
		// spreading it across every folder switch.
		if (!isVisibleMode()) {
			lastTreePaths = null;
			return;
		}
		// Switching to a different active folder invalidates the
		// incremental diff: the new folder's path-set is wholly
		// disjoint from the old one's, and a remove-everything
		// followed by an add-everything `batch` over Pierre's
		// child index burns enough time on tens-of-thousands-of-
		// files repos to be the dominant frame stall when the user
		// hits the folder bar. `resetPaths` rebuilds the path
		// store in one shot and skips the per-op event emission
		// `batch` runs; we also pay the cost once instead of twice
		// (the intermediate "previous folder cleared, new folder
		// not yet loaded" state used to trigger a wasted full
		// remove batch on its own). `workspace.activeFolderPath`
		// is reactive — Svelte re-runs us when it flips.
		const currentFolder = workspace.activeFolderPath;
		const folderSwitched = currentFolder !== lastEffectFolderPath;
		if (folderSwitched) {
			lastEffectFolderPath = currentFolder;
			lastTreePaths = null;
		}
		const nextSet = new Set(merged);
		// Force the wholesale-rebuild path whenever the previous
		// snapshot was empty and the next one isn't. `applyPathsDiff`
		// is a per-op `tree.batch([{type:'add', path}, …])` storm,
		// which on a single mid-sized repo (~80k paths) measured at
		// 6.8 s of main-thread time — Pierre eats every `add`
		// through its child-index bookkeeping and emits an event per
		// op. `resetPaths(merged)` on the same data lands closer to
		// 1 s because it builds the path store in one shot. The
		// initial `loadPaths` after a fresh mount and the post-
		// folder-switch fill both hit this case (the effect's first
		// run sees `prev=∅, next=loaded paths`), so this is the
		// dominant cost outside of folder switches and worth the
		// special-case.
		const wholesaleFill = !folderSwitched && lastTreePaths !== null && lastTreePaths.size === 0 && nextSet.size > 0;

		// Coalesce echo runs. The path-set effect's deps include
		// `workspace.paths`, `workspace.gitStatusEntries` (via
		// `deletedSignature`), `workspace.scmFilterPaths`, and
		// `workspace.activeFolderPath`. On a folder switch each of
		// these flips in its own microtask cycle, so the effect
		// re-runs 2–3 times per tree mode before the cascade
		// settles. Most of those runs produce an identical
		// `merged` because the relevant slice didn't actually
		// change — but Pierre's `resetPaths` / `applyPathsDiff`
		// don't know that, do the full rebuild, and the resulting
		// shadow-DOM churn shows up as 200+ ms style recalcs
		// downstream. A cheap structural-equality skip here cuts
		// the duplicated `fileTree.update` runs (observed: 4 per
		// folder switch → 2) and the cascading style recalcs they
		// drag in. The `lastTreePaths` cursor stays as-is so the
		// next *real* change still takes the right branch.
		if (!folderSwitched && !wholesaleFill && lastTreePaths !== null && pathSetsEqual(lastTreePaths, nextSet)) {
			// Selection may have shifted under us even when the
			// path set didn't change (Save As writes activePath
			// then later flips paths) — replay it before
			// returning, same as the non-skip branch below does.
			const target = untrack(() => workspace.activePath);
			applySelection(tree, target, { afterReset: true });
			return;
		}

		// Profiling: paired with `setActiveFolder` / `loadPaths`
		// timings in `state.svelte.ts`, see test plan 0076. We
		// only emit a log line on a folder switch / wholesale fill
		// or a sizable update so steady-state edits don't spam the
		// console. The `User Timing` measure is always emitted —
		// it's free in the absence of a Performance recording.
		const treeStart = performance.now();
		performance.mark('moon:fileTree.update.start');

		let strategy: 'resetPaths' | 'applyPathsDiff';
		if (lastTreePaths === null || wholesaleFill) {
			tree.resetPaths(merged);
			strategy = 'resetPaths';
		} else {
			applyPathsDiff(tree, lastTreePaths, nextSet, merged);
			strategy = 'applyPathsDiff';
		}
		lastTreePaths = nextSet;

		const treeDur = performance.now() - treeStart;
		performance.mark('moon:fileTree.update.end');
		performance.measure('moon:fileTree.update', 'moon:fileTree.update.start', 'moon:fileTree.update.end');
		if (folderSwitched || wholesaleFill || treeDur > 50) {
			// eslint-disable-next-line no-console
			console.info(
				`moon-ide: fileTree.update mode=${mode} folder=${currentFolder ?? '<none>'} ` +
					`paths=${merged.length} ${strategy}=${treeDur.toFixed(1)}ms`,
			);
		}

		// Replay the active path so Save As (which mutates
		// `activePath` *before* the new file lands in `paths`, with
		// an `await` between the two) doesn't end up with the new
		// row unselected. The activePath effect's only dep is
		// `activePath`, which didn't change here.
		const target = untrack(() => workspace.activePath);
		applySelection(tree, target, { afterReset: true });
	});

	/**
	 * Structural set equality. Pierre's path lists are typically
	 * 100s–1000s of strings and most echo runs are bit-for-bit
	 * identical, so a size + every-member check is enough. We
	 * never need a deep-order comparison because Pierre's path
	 * store is order-agnostic at the input boundary.
	 */
	function pathSetsEqual(a: ReadonlySet<string>, b: ReadonlySet<string>): boolean {
		if (a === b) {
			return true;
		}
		if (a.size !== b.size) {
			return false;
		}
		for (const value of a) {
			if (!b.has(value)) {
				return false;
			}
		}
		return true;
	}

	function currentPaths(): readonly string[] {
		if (mode === 'changes') {
			return untrack(() => workspace.scmFilterPaths);
		}
		return mergedPathsWithDeletions(
			untrack(() => workspace.paths),
			untrack(() => workspace.gitStatusEntries),
		);
	}

	/**
	 * Refresh `lazyDirs` from the two sources of "directory
	 * present in the tree but not yet enumerated":
	 *
	 * - gitignored directories the backend collapsed
	 *   (`node_modules/`, `target/`, …)
	 * - directories the recursive walk stopped at because they
	 *   sit beyond `MAX_TREE_DEPTH`
	 *
	 * A folder switch resets every lazy-load bucket because the
	 * new folder has its own ignore + depth-cap set; same-folder
	 * refreshes preserve `lazyLoaded` so already-expanded subtrees
	 * stay visible across watcher kicks.
	 */
	function seedLazyDirs(
		folder: string | null,
		entries: readonly GitStatusEntry[],
		depthCapped: readonly string[],
	): void {
		if (folder !== lazySeedFolderPath) {
			lazySeedFolderPath = folder;
			lazyLoaded = new Set();
		}
		const next = new Set<string>();
		for (const entry of entries) {
			if (entry.status === 'ignored' && entry.path.endsWith('/') && !lazyLoaded.has(entry.path)) {
				next.add(entry.path);
			}
		}
		for (const path of depthCapped) {
			if (path.endsWith('/') && !lazyLoaded.has(path)) {
				next.add(path);
			}
		}
		lazyDirs = next;
	}

	/**
	 * Union `merged` with `lazyLoaded` while preserving the input
	 * order — Pierre tolerates either order, but a stable merge
	 * makes the `applyPathsDiff` add list smaller on the next tick.
	 */
	function mergeLazyLoaded(merged: readonly string[]): readonly string[] {
		const out = new Set(merged);
		for (const path of lazyLoaded) {
			out.add(path);
		}
		return [...out];
	}

	/**
	 * Fetch one level of children for an expanded gitignored
	 * directory and batch-add them to Pierre. The user expanded
	 * `path` (a `node_modules/`-style entry); we hand back its
	 * direct children so the tree can paint them. Sub-directories
	 * become themselves lazy entries — drilling deeper re-issues
	 * this command at the next level. Errors flash and leave the
	 * directory marked as still-lazy so a retry-click works.
	 */
	async function loadLazyDir(path: string): Promise<void> {
		if (!path.endsWith('/')) {
			return;
		}
		if (lazyLoading.has(path)) {
			return;
		}
		const local = tree;
		if (!local) {
			return;
		}
		lazyLoading.add(path);
		try {
			// `max_depth=0` means "direct children only" — the
			// walker pushes the entries of `path` but never
			// recurses into sub-directories. Drilling deeper
			// fires another lazy-load against the deeper rel.
			// `depth_capped` reflects the same walk: any direct
			// child that itself has hidden descendants comes back
			// in that list and gets added to `lazyDirs` so the
			// next click drills further.
			const result = await ipc.fs.collectPathsUnder(path, 0);
			const children = result.paths;
			const cappedChildren = new Set(result.depth_capped);
			if (children.length === 0) {
				lazyDirs.delete(path);
				lazyLoaded.add(path);
				return;
			}
			const addPaths: string[] = [];
			for (const child of children) {
				if (child === path) {
					continue;
				}
				if (lazyLoaded.has(child)) {
					continue;
				}
				addPaths.push(child);
				lazyLoaded.add(child);
				// `max_depth=0` means we only learn whether a
				// child *itself* has hidden descendants via the
				// `depth_capped` list. Anything not in that set
				// is fully enumerated as far as the walker can
				// see — it's either a leaf or its contents are
				// already in `children`. Marking only the capped
				// ones as lazy avoids pointless IPC roundtrips
				// when the user expands an empty directory.
				if (child.endsWith('/') && cappedChildren.has(child)) {
					lazyDirs.add(child);
				}
			}
			lazyDirs.delete(path);
			lazyLoaded.add(path);
			if (addPaths.length > 0) {
				const ops: FileTreeBatchOperation[] = addPaths.map((p) => ({ type: 'add', path: p }));
				try {
					local.batch(ops);
					// Mirror the new paths into `lastTreePaths`
					// so the next `applyPathsDiff` run (driven
					// by a git-status refresh or path-set
					// effect re-fire) sees them as already
					// present rather than trying to re-add and
					// crashing Pierre's path store. The set is
					// recreated rather than mutated so any
					// future reactive dependency on identity
					// (currently none, but cheap insurance)
					// still triggers.
					if (lastTreePaths !== null) {
						const next = new Set(lastTreePaths);
						for (const p of addPaths) {
							next.add(p);
						}
						lastTreePaths = next;
					}
				} catch {
					// Pierre rejected the batch (path-store
					// invariant violation); roll the lazy-load
					// state back so a subsequent click can retry
					// rather than silently leaving the user with
					// an empty expanded folder.
					for (const p of addPaths) {
						lazyLoaded.delete(p);
						if (p.endsWith('/')) {
							lazyDirs.add(p);
						}
					}
					lazyDirs.add(path);
					lazyLoaded.delete(path);
				}
			}
		} catch (err) {
			workspace.flash(`Could not load ${path}: ${formatError(err)}`);
		} finally {
			lazyLoading.delete(path);
		}
	}

	/**
	 * Probe whether `path` is a still-unloaded ignored directory
	 * and, if so, kick the one-level walk. Called from the click /
	 * keyboard handlers after Pierre has had a chance to flip the
	 * row's expansion state.
	 */
	function maybeLoadLazyAt(path: string): void {
		if (mode !== 'all') {
			return;
		}
		if (!lazyDirs.has(path)) {
			return;
		}
		const item = tree?.getItem(path);
		if (!item || !item.isDirectory()) {
			return;
		}
		// Pierre's discriminated union narrows via `isDirectory()`'s
		// literal-`true` return type, but svelte-check's type
		// inference doesn't pick it up here — cast through after
		// the early-return guard.
		const dir = item as FileTreeDirectoryHandle;
		if (!dir.isExpanded()) {
			return;
		}
		void loadLazyDir(path);
	}

	/**
	 * Apply the symmetric diff of `prev` and `next` to `local` via a
	 * single `batch` call. Removes are emitted deepest-first because
	 * Pierre's `removePath` throws if the path doesn't exist — when
	 * a directory and its descendants both disappear from the walk,
	 * removing the descendants first leaves the directory empty so
	 * its own removal lands cleanly. Adds run in any order; Pierre's
	 * `addPath` creates intermediate parent segments as needed, but
	 * shortest-first keeps the path-store's internal node-creation
	 * count deterministic.
	 *
	 * On any internal Pierre error the whole batch is rolled back
	 * and we fall back to `resetPaths(merged)` so the tree's
	 * displayed paths still match `merged`. Expansion state is lost
	 * in that fallback path (same semantics as before this change),
	 * but the tree is at least correct.
	 */
	function applyPathsDiff(
		local: FileTree,
		prev: ReadonlySet<string>,
		next: ReadonlySet<string>,
		merged: readonly string[],
	) {
		const removed: string[] = [];
		const added: string[] = [];
		for (const path of prev) {
			if (!next.has(path)) {
				removed.push(path);
			}
		}
		for (const path of next) {
			if (!prev.has(path)) {
				added.push(path);
			}
		}
		if (removed.length === 0 && added.length === 0) {
			return;
		}
		// `'changes'` mode: the path list never contains directory
		// entries (only the changed file paths). Pierre's
		// `removePath` doesn't auto-prune empty parents — its
		// `promoteEmptyAncestorsToExplicit` flips the now-empty
		// directory node to "explicit" so it stays visible. In
		// `'all'` mode that's correct (empty dirs exist on disk
		// and should render), but here a committed-or-reverted
		// file leaves its parent directory dangling in the tree
		// with no children. Walk the ancestors of every removed
		// path; any ancestor that no longer has a descendant in
		// `next` gets queued for removal too, deepest-first so
		// nested empty chains unravel cleanly. `'all'` mode keeps
		// receiving directory paths from the filesystem walk and
		// doesn't need this pass.
		if (mode === 'changes' && removed.length > 0) {
			const needed = new Set<string>();
			for (const path of next) {
				appendAncestors(path, needed);
			}
			// `'changes'` mode never feeds Pierre directory entries
			// directly (only file paths from `gitStatusEntries`),
			// so `prev` won't list the dirs we need to clean up —
			// but Pierre auto-created those dir nodes on `addPath`
			// when each file landed, so they're guaranteed to be
			// present at removal time. No extra `prev.has` guard
			// needed; the depth-descending sort below ensures the
			// file leaf comes out before its parent dir.
			const orphanDirs = new Set<string>();
			for (const path of removed) {
				const ancestors: string[] = [];
				appendAncestorList(path, ancestors);
				for (const ancestor of ancestors) {
					if (needed.has(ancestor)) {
						break;
					}
					orphanDirs.add(ancestor);
				}
			}
			for (const dir of orphanDirs) {
				removed.push(dir);
			}
		}
		removed.sort((a, b) => depthOf(b) - depthOf(a));
		added.sort((a, b) => depthOf(a) - depthOf(b));
		const ops: FileTreeBatchOperation[] = [];
		for (const path of removed) {
			ops.push({ type: 'remove', path });
		}
		for (const path of added) {
			ops.push({ type: 'add', path });
		}
		try {
			local.batch(ops);
		} catch (err) {
			// eslint-disable-next-line no-console
			console.warn('moon-ide: incremental tree update failed; falling back to full reset', err);
			local.resetPaths(merged);
		}
	}

	/** Add every ancestor directory of `path` (each ending in `/`)
	 *  to `out`. `foo/bar/baz.txt` → `foo/bar/`, `foo/`. Top-level
	 *  files contribute nothing. */
	function appendAncestors(path: string, out: Set<string>): void {
		let idx = path.length;
		while (idx > 0) {
			const slash = path.lastIndexOf('/', idx - 1);
			if (slash < 0) {
				return;
			}
			const ancestor = path.slice(0, slash + 1);
			if (out.has(ancestor)) {
				return;
			}
			out.add(ancestor);
			idx = slash;
		}
	}

	function appendAncestorList(path: string, out: string[]): void {
		let idx = path.length;
		while (idx > 0) {
			const slash = path.lastIndexOf('/', idx - 1);
			if (slash < 0) {
				return;
			}
			out.push(path.slice(0, slash + 1));
			idx = slash;
		}
	}

	function depthOf(path: string): number {
		// `foo/bar/` and `foo/bar` both report depth 2 — trailing
		// slashes only mean "this is a directory" and shouldn't
		// inflate the count by an empty segment.
		let depth = 0;
		for (let i = 0; i < path.length; i++) {
			if (path[i] === '/' && i < path.length - 1) {
				depth++;
			}
		}
		return depth + 1;
	}

	// Stable string summary of the deleted-path subset. Changes if
	// and only if the set of deleted paths changes — add/modify/
	// untracked refreshes produce the same signature, so the
	// dependent effect doesn't re-run.
	function deletedPathsSignature(entries: readonly GitStatusEntry[]): string {
		const deleted: string[] = [];
		for (const entry of entries) {
			if (entry.status === 'deleted') {
				deleted.push(entry.path);
			}
		}
		deleted.sort();
		return deleted.join('\0');
	}

	// Mirror of `deletedPathsSignature` for the `Ignored` subset.
	// Drives the lazy-seed re-run when a background `refreshGitStatus`
	// lands ignored entries the initial pass didn't have. Without
	// this, the path-set effect would only re-run on path / deletion
	// changes, and `seedLazyDirs` would stay anchored to whatever
	// (likely empty) set of ignored entries existed at first paint —
	// `node_modules/` would never get the lazy badge so the click
	// would no-op. See `seedLazyDirs` and `loadLazyDir` for the load
	// path that depends on this.
	function ignoredPathsSignature(entries: readonly GitStatusEntry[]): string {
		const ignored: string[] = [];
		for (const entry of entries) {
			if (entry.status === 'ignored') {
				ignored.push(entry.path);
			}
		}
		ignored.sort();
		return ignored.join('\0');
	}

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
		// Gate Pierre's `onSelectionChange` callback so it skips the
		// per-mode click policy for this programmatic mirror — only
		// genuine user clicks (routed via `onTreeClick`) should flip
		// diff mode on. See `syncingSelection` above. Pierre fires
		// the callback synchronously from `.select()` / `.deselect()`,
		// so a simple flag scoped around the dispatch suffices; the
		// `try / finally` keeps it correct even if Pierre throws.
		syncingSelection = true;
		try {
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
		} finally {
			syncingSelection = false;
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
		// Both modes stay mounted (CSS-toggled visibility), so the
		// focus shortcut needs to land in the *visible* tree, not
		// in whichever instance happened to subscribe last. The
		// scmFilterOn read is untracked for the same reason as
		// activePath above — focusTick is the sole trigger.
		const filterOn = untrack(() => workspace.scmFilterOn);
		if ((mode === 'changes') !== filterOn) {
			return;
		}
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
		// Pierre handles ArrowRight (expand) and ArrowLeft / Space
		// (toggle) on its row before this wrapper-level handler
		// fires, so by the time we read `isExpanded()` the new
		// state is in place. Probe lazy load on every keystroke
		// that could have flipped expansion.
		if (event.key === 'ArrowRight' || event.key === 'ArrowLeft' || event.key === ' ') {
			const focused = tree?.getFocusedPath();
			if (focused) {
				queueMicrotask(() => maybeLoadLazyAt(focused));
			}
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

	/**
	 * Apply the per-mode "tree click" semantics to `path`:
	 *
	 *  - `'all'` mode: clear any sticky diff-mode flag for this
	 *    path (so a click in the regular tree always lands in
	 *    Source view, even if the path was previously toggled to
	 *    Diff via the changes-view click, the gutter, or the tab
	 *    toolbar). Then open the file and hand focus to the
	 *    editor — typing right after the click should edit the
	 *    file, not seed Pierre's tree search.
	 *  - `'changes'` mode: set the diff-mode flag *only* for files
	 *    with `modified` status — those are the rows where a
	 *    side-by-side comparison has something to show. Added /
	 *    untracked / deleted files don't have two sides to
	 *    compare (added: no HEAD blob; deleted: no working tree;
	 *    untracked: same as added), so the diff view collapses
	 *    into a single-coloured wall of plus / minus prefixes
	 *    that wastes a pane. For those, clear the flag like the
	 *    `'all'` branch does and let the regular Editor /
	 *    deleted-file read-only view render instead. Same
	 *    `EditorTabs.canDiff` rule, applied at click time.
	 *
	 * Both `setDiffMode` and `openFile` are idempotent for "no
	 * actual change" inputs, so this is safe to call from both
	 * the `onSelectionChange` callback (fires only when selection
	 * actually changes — a click on the already-selected row is
	 * silently dropped by Pierre's selection-version check) and
	 * the wrapper-click listener below (fires on every click,
	 * including on the active row).
	 *
	 * We default to `focus: true` (the `openFile` default) so the
	 * editor's focusTick effect pulls DOM focus onto CodeMirror.
	 * Arrow-key navigation inside the tree is unaffected — Pierre
	 * only fires `onSelectionChange` on click, not on focus
	 * movement, so arrow keys still move Pierre's row cursor
	 * without re-triggering this function.
	 */
	function activateRowFromTree(path: string) {
		const item = tree?.getItem(path);
		if (!item || item.isDirectory()) {
			return;
		}
		// Review-changes pseudo-tab is open and we're in the
		// changes-only tree: route the click to a "scroll my
		// section into view" signal on `WorkspaceState` instead
		// of opening a new editor tab. The review view is the
		// aggregated diff for *this exact file list*, so the
		// click "selects" the file inside the open review rather
		// than spawning a per-file diff next to it. Plain tree
		// (`mode === 'all'`) keeps its open-as-editor-tab
		// behaviour — review never appears there.
		if (mode === 'changes' && workspace.isReviewTabVisible) {
			workspace.requestReviewScroll(path);
			return;
		}
		const status = workspace.gitStatusEntries.find((e) => e.path === path)?.status;
		const wantsDiff = mode === 'changes' && status === 'modified';
		workspace.setDiffMode(path, wantsDiff);
		void workspace.openFile(path);
	}

	/**
	 * Wrapper-level click handler. Pierre's `onSelectionChange`
	 * only fires when the selection version increments, so a
	 * click on the already-selected row is otherwise invisible to
	 * us — and that path used to leave the file stuck in whatever
	 * view mode it had on its previous open. Listening here too
	 * (with `composedPath` to pierce Pierre's shadow root) lets us
	 * apply the same per-mode reset on every click, regardless of
	 * whether selection changed.
	 */
	function onTreeClick(event: MouseEvent) {
		// Sidebar's `pointer-events: none` on the hidden pane
		// already blocks real user clicks from reaching the
		// off-screen tree, but mirroring the visibility check here
		// keeps the contract uniform with `onSelectionChange` —
		// any future "programmatic dispatchEvent into the tree"
		// caller can't accidentally trip the wrong mode.
		if (!isVisibleMode()) {
			return;
		}
		const path = pathFromComposedClick(event);
		if (path === null) {
			return;
		}
		activateRowFromTree(path);
		// Pierre's row-level click handler already toggled
		// expansion synchronously by the time the event bubbled
		// here, so `isExpanded()` reflects post-click state. If
		// the row is one of our gitignored-and-not-yet-walked
		// dirs and just opened, fetch its direct children.
		maybeLoadLazyAt(path);
	}

	function isVisibleMode(): boolean {
		const active = workspace.scmFilterOn ? 'changes' : 'all';
		return mode === active;
	}

	function pathFromComposedClick(event: MouseEvent): string | null {
		for (const node of event.composedPath()) {
			if (!(node instanceof Element)) {
				continue;
			}
			const path = node.getAttribute('data-item-path');
			if (path !== null && path.length > 0) {
				return path;
			}
		}
		return null;
	}
</script>

<!-- svelte-ignore a11y_no_static_element_interactions -->
<div class="tree" bind:this={treeMount} onkeydown={onKeyDown} ondblclick={onDblClick} onclick={onTreeClick}></div>

<style>
	.tree {
		height: 100%;
		width: 100%;
		overflow: hidden;
		--trees-row-font-family: var(--m-font-ui);
	}
</style>
