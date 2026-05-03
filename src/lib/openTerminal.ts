// One-shot helpers to spawn a new terminal from anywhere in
// the UI. The launcher popover and the bottom-panel quick
// buttons both call these so cwd resolution and the
// "container down → no-op" rule live in one place.
//
// Architecture: ADR 0009.

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
	const id = workspace.workspace?.id;
	if (!id || container.state !== 'running') {
		return;
	}
	const folder = workspace.activeFolder;
	const cwd = folder ? containerCwdFor(folder.path) : '/workspace';
	const target: TerminalTarget = {
		kind: 'container',
		workspace_id: id,
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
