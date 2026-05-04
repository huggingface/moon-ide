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
	} from '@pierre/trees';
	import ContextMenu from './ContextMenu.svelte';
	import type { ContextMenuItem } from './contextMenu';
	import { workspace } from '../state.svelte';
	import type { GitFileStatus, GitStatusEntry } from '../protocol';

	let treeMount: HTMLDivElement;
	let tree: FileTree | undefined;

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
	const PIERRE_OVERRIDES_CSS = `
[data-item-section='git'] {
	margin-right: 8px;
}
`;

	onMount(() => {
		if (!treeMount) {
			return;
		}
		tree = new FileTree({
			paths: mergedPathsWithDeletions(
				untrack(() => workspace.paths),
				untrack(() => workspace.gitStatusEntries),
			),
			flattenEmptyDirectories: true,
			unsafeCSS: PIERRE_OVERRIDES_CSS,
			// Start every folder collapsed. The previous default
			// (`initialExpansion: 1`) eagerly opened gitignored folders
			// like `node_modules/` / `target/` before we knew they were
			// ignored, and once we did there was no clean "uncollapse
			// only the non-ignored ones" story that didn't fight the
			// user's own expansions. A fully-collapsed root is the
			// simpler default; users open what they actually want.
			initialExpansion: 0,
			search: true,
			gitStatus: untrack(() => workspace.gitStatusEntries),
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
		tree.render({ containerWrapper: treeMount });
		applyGitOverlay(
			tree,
			untrack(() => workspace.gitStatusEntries),
		);
		return () => {
			disposeActiveMenu();
			tree?.cleanUp();
			tree = undefined;
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

	// Map a row + its backend git status to menu items. Kept small
	// and deliberately asymmetric: "Discard changes" is the reason
	// this menu exists, "Copy path" is a staple discovery tool. If a
	// third action shows up and proves useful we'll add it; until
	// then more items is more cognitive load for worse focus.
	function buildMenuItems(item: PierreContextMenuItem): ContextMenuItem[] {
		const items: ContextMenuItem[] = [];
		if (item.kind === 'file' && canViewDiff(item.path)) {
			items.push({
				id: 'view-diff',
				label: 'View diff',
				onSelect: () => {
					// Opens a dedicated diff tab alongside any
					// existing editor tab for this file, so
					// `Alt+Left` walks back to the regular editor
					// view. Idempotent: if a diff tab is already
					// open for this path, it gets focused rather
					// than duplicated.
					void workspace.openDiffTab(item.path);
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
			// tracks that.
			const entry = workspace.gitStatusEntries.find((e) => e.path === item.path);
			if (entry?.status === 'untracked') {
				return 'Discard (move untracked file to trash)';
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
		tree.setGitStatus(entries);
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
			const stripped = entry.path.replace(/\/+$/, '');
			const segments = stripped.split('/');
			let cumulative = '';
			for (let i = 0; i < segments.length - 1; i++) {
				const seg = segments[i] ?? '';
				cumulative = cumulative === '' ? seg : `${cumulative}/${seg}`;
				const key = `${cumulative}/`;
				if (entry.status === 'ignored') {
					ignoredAncestors.add(key);
					continue;
				}
				const existing = folderSeverity.get(key);
				if (existing === undefined || SEVERITY[entry.status] > SEVERITY[existing]) {
					folderSeverity.set(key, entry.status);
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
	// path model), so we gate it to changes that actually restructure
	// the tree: the filesystem list, plus the set of deleted ghost
	// rows. We deliberately read `gitStatusEntries` via a scoped
	// derived so a pure add/modify refresh (no deletion change)
	// doesn't trigger a rebuild — `setGitStatus` above handles those
	// in-place.
	const deletedSignature = $derived(deletedPathsSignature(workspace.gitStatusEntries));

	// Reactively reset paths when the workspace path list (or its
	// deleted-row set) changes, then replay the active path so
	// Save As (which mutates `activePath` *before* the new file
	// lands in `paths`, with an `await` between the two) doesn't end
	// up with the new row unselected, the keyboard cursor stuck on
	// row 0, and the list scrolled to the top.
	//
	// We can't rely on the activePath effect to re-fire here — its
	// only dep is `activePath`, which didn't change.
	$effect(() => {
		const paths = workspace.paths;
		// Track the signature so a deletion appearing/disappearing
		// re-runs this effect. We immediately untrack to re-read
		// `gitStatusEntries` for the merge — the signature already
		// captured "what changed".
		void deletedSignature;
		if (!tree) {
			return;
		}
		const entries = untrack(() => workspace.gitStatusEntries);
		tree.resetPaths(mergedPathsWithDeletions(paths, entries));
		const target = untrack(() => workspace.activePath);
		applySelection(tree, target, { afterReset: true });
	});

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
<div class="tree" bind:this={treeMount} onkeydown={onKeyDown} ondblclick={onDblClick}></div>

<style>
	.tree {
		height: 100%;
		width: 100%;
		overflow: hidden;
		--trees-row-font-family: var(--m-font-ui);
	}
</style>
