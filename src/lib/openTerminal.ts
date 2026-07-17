// One-shot helpers to spawn a new terminal from anywhere in
// the UI. The launcher popover and the bottom-panel quick
// buttons both call these so cwd resolution and the
// "container down → no-op" rule live in one place.
//
// Architecture: ADR 0009.

import { bottomPanel, type TerminalTab } from './bottomPanel.svelte';
import { container } from './container.svelte';
import { workspace } from './state.svelte';
import { terminal as terminalStore } from './terminal.svelte';
import type { TerminalTarget } from './protocol';

const DEFAULT_COLS = 80;
const DEFAULT_ROWS = 24;

/** Folder path → the bottom-panel terminal id that was active
 *  the last time the user worked in that folder.
 *
 *  Populated by [`rememberActiveTerminalFor`] at folder-switch
 *  time and consulted by [`ensureActiveFolderTerminal`] before
 *  it falls back to cwd-matching / spawning. The point is to
 *  preserve "I had terminal #3 selected in project A when I
 *  switched away" so the same terminal lights up when the user
 *  returns to A — not just any terminal whose cwd happens to
 *  match.
 *
 *  Module-local on purpose: the policy lives next to
 *  `ensureActiveFolderTerminal`, and the only callers are the
 *  workspace state machine. Not persisted across launches —
 *  PTYs don't survive a restart and the bottom panel
 *  deliberately doesn't replay tabs (see `BottomPanelStore`
 *  comment), so a remembered id would point at nothing on
 *  next boot.
 *
 *  Stale entries (pointing at a since-closed terminal, or at a
 *  folder that's been unbound) are pruned lazily on read by
 *  `ensureActiveFolderTerminal`; folder removal calls
 *  [`forgetTerminalMemoryFor`] eagerly so the map doesn't grow
 *  without bound across a long session of bind / unbind cycles. */
const lastTerminalByFolder = new Map<string, string>();

/** Open a host terminal rooted at the active folder
 * (or `$HOME` when no folder is selected). */
export function openHostTerminal(): void {
	const target: TerminalTarget = {
		kind: 'host',
		cwd: workspace.activeFolder?.path ?? null,
	};
	void terminalStore.open(target, DEFAULT_COLS, DEFAULT_ROWS);
}

/** True when a container terminal can actually be spawned —
 * i.e. the workspace has an id and its container is up.
 *
 * Synchronous; reads whatever `container.state` happens to hold
 * right now. Callers that race the startup probe (terminal
 * spawn helpers in particular) should `await
 * container.awaitRefreshed()` first — otherwise the pre-resolve
 * `null` state reads as "not running" and they silently fall
 * back to host. */
export function canOpenContainerTerminal(): boolean {
	return container.state === 'running' && workspace.workspace?.id !== undefined;
}

/** Open a terminal in the preferred environment: container when
 *  the workspace shell is up, host otherwise. Awaits any
 *  in-flight `container.refresh()` first so a click that lands
 *  during the startup probe doesn't get host'd just because
 *  `container.state` hasn't resolved yet, then the launch-time
 *  auto-resume gate — mid-resume a refresh truthfully reports
 *  `stopped`, which is exactly the window where "prefer the
 *  container" must wait rather than fall back to host.
 *
 *  This is the helper user-driven spawns should reach for. The
 *  bottom-panel quick-host / quick-container buttons (where the
 *  user has already picked) and the launcher popover (where the
 *  popover is gated on a known state) call the specific helpers
 *  directly. */
export async function openPreferredTerminal(): Promise<void> {
	await container.awaitRefreshed();
	if (canOpenContainerTerminal()) {
		openContainerTerminal();
		return;
	}
	openHostTerminal();
}

/** Open a container terminal at `/workspace/<active-folder>`,
 * falling back to `/workspace` when nothing is selected. No-op
 * when the workspace container isn't running, so callers can
 * always invoke this from a click handler — disabling the
 * trigger is a UX nicety, not a safety requirement. */
export function openContainerTerminal(): void {
	if (container.state !== 'running') {
		return;
	}
	const folder = workspace.activeFolder;
	const cwd = folder ? containerCwdFor(folder.path) : '/workspace';
	const target: TerminalTarget = {
		kind: 'container',
		cwd,
	};
	void terminalStore.open(target, DEFAULT_COLS, DEFAULT_ROWS);
}

// `/home/me/code/moon-landing` → `/workspace/moon-landing`.
// Mirrors `moon_terminal::TerminalTarget::container_cwd_for_folder`
// — keep the two in sync.
//
// Worktree-backed folders (ADR 0029) are the exception: they live
// inside the parent repo at `<parent>/.worktrees/<slug>`, so they
// ride the parent's bind mount — their container path is the parent's
// `/workspace/<parent-basename>` mount plus the relative tail, not a
// mount of their own. Without this a worktree terminal `chdir`s to a
// path that doesn't exist in the container. Mirrors
// `moon_core::worktree::worktree_container_path`.
export function containerCwdFor(absolutePath: string): string {
	const normalised = absolutePath.replace(/\/+$/, '');
	const folder = workspace.workspace?.folders.find((f) => f.path === absolutePath || f.path === normalised);
	if (folder?.origin.kind === 'worktree') {
		const parent = folder.origin.parentPath.replace(/\/+$/, '');
		if (normalised.startsWith(`${parent}/`)) {
			const tail = normalised.slice(parent.length + 1);
			const parentBasename = parent.slice(parent.lastIndexOf('/') + 1);
			return `/workspace/${parentBasename}/${tail}`;
		}
	}
	const basename = normalised.slice(normalised.lastIndexOf('/') + 1);
	if (basename.length === 0) {
		return '/workspace';
	}
	return `/workspace/${basename}`;
}

/** Called after a project (active folder) switch *or* an
 * `openLocal(newFolder)`: if the bottom panel is visible and
 * already hosts at least one terminal, make sure the user lands
 * on a terminal rooted in the (now-)active folder.
 *
 * Strategy, in order:
 * 1. **Per-folder memory.** If the user had a terminal selected
 *    the last time this folder was active and it's still alive,
 *    re-focus it. Lets you bounce between projects without losing
 *    "I was in pane #3 over here". Populated by
 *    [`rememberActiveTerminalFor`] from the workspace state machine
 *    on the way out of a folder.
 * 2. **cwd match.** Otherwise, look for any live (non-exited)
 *    terminal whose cwd matches the active folder — host
 *    (`target.cwd === folderPath`) or container
 *    (`target.cwd === containerCwdFor(folderPath)`). First match
 *    wins; we don't care which mode, the user's existing setup
 *    trumps our default preference.
 * 3. **Spawn.** None of the above — open a fresh one in the
 *    preferred mode: container when the workspace container is
 *    up, host otherwise.
 *
 * No-op when the panel is hidden or hosts no terminals at all —
 * we don't surprise users who collapsed the strip or only have
 * log / diag tabs open. Initial workspace hydration also calls
 * `setActiveFolder` indirectly, but the entry point that wires
 * this in skips that path (see `WorkspaceState.setActiveFolder`). */
export function ensureActiveFolderTerminal(): void {
	if (!bottomPanel.visible) {
		return;
	}
	const tabs = bottomPanel.tabs;
	const hasAnyTerminal = tabs.some((t) => t.kind === 'terminal');
	if (!hasAnyTerminal) {
		return;
	}
	const folder = workspace.activeFolder;
	if (!folder) {
		return;
	}
	const folderPath = folder.path;
	pruneClosedMemoryEntries();
	const remembered = lastTerminalByFolder.get(folderPath);
	if (remembered !== undefined) {
		const tab = tabs.find((t) => t.id === remembered);
		if (tab && tab.kind === 'terminal') {
			const session = terminalStore.sessionFor(remembered);
			if (!session?.closed) {
				bottomPanel.setActive(remembered);
				return;
			}
		}
		// Remembered terminal is dead / gone — drop it so the
		// next lookup falls through cleanly.
		lastTerminalByFolder.delete(folderPath);
	}
	const containerCwd = containerCwdFor(folderPath);
	const existing = tabs.find((t): t is TerminalTab => {
		if (t.kind !== 'terminal') {
			return false;
		}
		const expectedCwd = t.target.kind === 'host' ? folderPath : containerCwd;
		if (t.target.cwd !== expectedCwd) {
			return false;
		}
		// Skip exited terminals — re-using a dead PTY isn't a
		// thing; the user wants a live shell on the new folder.
		const session = terminalStore.sessionFor(t.id);
		if (session?.closed) {
			return false;
		}
		return true;
	});
	if (existing) {
		bottomPanel.setActive(existing.id);
		return;
	}
	// Spawn a fresh one in the preferred mode. `openPreferredTerminal`
	// awaits the in-flight container refresh so a folder switch
	// triggered before the startup probe resolves still picks the
	// container when it's up.
	void openPreferredTerminal();
}

/** Snapshot the bottom panel's currently-active terminal as the
 *  remembered pick for `folderPath`. Called by the workspace
 *  state machine right before it flips to a new active folder,
 *  so the next time the user returns to `folderPath` the same
 *  terminal lights up.
 *
 *  No-op when:
 *  - `folderPath` is `null` (no folder was active before the
 *    switch — e.g. the very first folder being bound),
 *  - the active bottom-panel tab is something else (a log /
 *    diag tab the user clicked while in this folder); we
 *    deliberately leave any prior remembered terminal entry
 *    untouched in that case, on the assumption that the user
 *    still wants "their terminal" back when they return to
 *    this folder.
 *
 *  Note we don't record on every `bottomPanel.setActive` call:
 *  doing so would require this module to observe panel state
 *  and pull in a `$state` subscription. Snapshotting at
 *  folder-switch time covers the only case where the value is
 *  actually read (the same folder being re-entered later). */
export function rememberActiveTerminalFor(folderPath: string | null): void {
	if (folderPath === null) {
		return;
	}
	const id = bottomPanel.activeId;
	if (id === null) {
		return;
	}
	const tab = bottomPanel.tabs.find((t) => t.id === id);
	if (!tab || tab.kind !== 'terminal') {
		return;
	}
	lastTerminalByFolder.set(folderPath, id);
}

/** Drop the remembered-terminal entry for `folderPath`. Called
 *  by `WorkspaceState.removeFolder` so unbinding a folder
 *  doesn't leave a dangling id behind to be lazily pruned
 *  later. */
export function forgetTerminalMemoryFor(folderPath: string): void {
	lastTerminalByFolder.delete(folderPath);
}

/** Lazy pruning: drop entries whose terminal id no longer
 *  exists in the panel (closed by the user, supervisor lost
 *  it, …). Called from `ensureActiveFolderTerminal` so a typical
 *  read pays at most one O(tabs + entries) sweep. */
function pruneClosedMemoryEntries(): void {
	if (lastTerminalByFolder.size === 0) {
		return;
	}
	const alive = new Set(bottomPanel.tabs.filter((t) => t.kind === 'terminal').map((t) => t.id));
	for (const [folder, id] of lastTerminalByFolder) {
		if (!alive.has(id)) {
			lastTerminalByFolder.delete(folder);
		}
	}
}
