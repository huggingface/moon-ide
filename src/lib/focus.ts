// Region cycling + direct region focus, shared between the F6 / Ctrl+0
// keybindings (App.svelte) and the corresponding palette commands. Lives
// in its own module — not in `state.svelte.ts` — because the cycle order
// depends on the DOM layout (sidebar always present, right pane only
// when split, etc.) rather than on persisted application state.
//
// Each region has a `data-region` attribute on its root element. The
// cycle walks an order computed from the current layout, asks the
// matching component to pull focus in via a ticker on `WorkspaceState`,
// and lets the component decide *what* to focus (Pierre Trees row,
// CodeMirror view, theme button…).
import { workspace } from './state.svelte';

export type Region = 'sidebar' | 'editor-left' | 'editor-right' | 'status';

export function regionOrder(): Region[] {
	const list: Region[] = ['sidebar'];
	if (workspace.workspace) {
		list.push('editor-left');
		if (workspace.hasSplit) {
			list.push('editor-right');
		}
	}
	list.push('status');
	return list;
}

export function currentRegion(): Region | null {
	const ae = document.activeElement;
	if (!(ae instanceof HTMLElement)) {
		return null;
	}
	const host = ae.closest<HTMLElement>('[data-region]');
	const id = host?.dataset.region;
	if (id === 'sidebar' || id === 'editor-left' || id === 'editor-right' || id === 'status') {
		return id;
	}
	return null;
}

export function focusRegion(target: Region) {
	if (target === 'sidebar') {
		workspace.requestSidebarFocus();
		return;
	}
	if (target === 'status') {
		workspace.requestStatusFocus();
		return;
	}
	const side = target === 'editor-left' ? 'left' : 'right';
	workspace.focusSide(side);
	workspace.requestEditorFocus();
}

export function cycleFocus(forward: boolean) {
	const order = regionOrder();
	if (order.length === 0) {
		return;
	}
	const current = currentRegion();
	// When focus is somewhere off-region (palette, dialog, body) F6
	// enters the cycle at the start and Shift+F6 at the end, so the
	// first press always lands on a sensible edge of the order.
	let next: Region | undefined;
	if (current === null) {
		next = forward ? order[0] : order[order.length - 1];
	} else {
		const idx = order.indexOf(current);
		const step = forward ? 1 : -1;
		next = order[(idx + step + order.length) % order.length];
	}
	if (next) {
		focusRegion(next);
	}
}
