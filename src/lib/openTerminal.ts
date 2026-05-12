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
 * i.e. the workspace has an id and its container is up. */
export function canOpenContainerTerminal(): boolean {
	return container.state === 'running' && workspace.workspace?.id !== undefined;
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
export function containerCwdFor(absolutePath: string): string {
	const normalised = absolutePath.replace(/\/+$/, '');
	const basename = normalised.slice(normalised.lastIndexOf('/') + 1);
	if (basename.length === 0) {
		return '/workspace';
	}
	return `/workspace/${basename}`;
}

/** Called after a project (active folder) switch: if the bottom
 * panel is visible and already hosts at least one terminal, make
 * sure the user lands on a terminal rooted in the **new** folder.
 *
 * Strategy:
 * 1. Look for a live (non-exited) terminal whose cwd matches the
 *    new folder — either host (`target.cwd === folderPath`) or
 *    container (`target.cwd === containerCwdFor(folderPath)`).
 *    First match wins; we don't care whether it's host or
 *    container, the user's existing setup trumps our default
 *    preference.
 * 2. If none, spawn a fresh one in the preferred mode: container
 *    when the workspace container is up, host otherwise.
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
	if (canOpenContainerTerminal()) {
		openContainerTerminal();
		return;
	}
	openHostTerminal();
}
