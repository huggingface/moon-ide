// Reactive state for the workspace picker palette
// (`Ctrl+Shift+O`). Lists every workspace in the catalog,
// most-recently-active first; selecting one calls
// `window_open(id)` which either sends a focus message to the
// sibling process that already owns the workspace or spawns
// a fresh `moon-ide --workspace <slug>` child.
//
// A "Forget" affordance per row calls `workspace_delete(slug)`
// to drop the workspace from the catalog (and tear down its
// compose project + state dir). The current process's
// workspace is filtered out of the deletable set; the backend
// also refuses to delete a workspace whose instance lock is
// held by a live sibling process.
//
// A "Rename" affordance per row swaps the row's label for an
// inline input (Enter commits, Escape cancels). Renaming only
// touches the display name — the slug (state dir, compose
// project, instance socket) is immutable, so a live sibling
// process keeps working untouched; it picks the new name up on
// next launch.

import { ipc } from './ipc';
import { formatError } from './protocol';
import type { WorkspaceMeta } from './protocol';
import { workspace } from './state.svelte';
import { currentWorkspaceId } from './workspace-id';
import { applyWorkspaceScheme } from './workspaceTheme';

class WorkspacePickerStore {
	visible = $state(false);
	query = $state('');
	entries = $state<WorkspaceMeta[]>([]);
	error = $state<string | null>(null);
	loading = $state(false);
	selectedIndex = $state(0);
	/** Id of the row currently being inline-renamed; `null` when no rename is in flight. */
	renamingId = $state<string | null>(null);
	/** Draft text of the inline rename input. */
	renameDraft = $state('');

	get filtered(): WorkspaceMeta[] {
		const q = this.query.trim().toLowerCase();
		if (q.length === 0) {
			return this.entries;
		}
		return this.entries.filter((m) => m.id.toLowerCase().includes(q) || m.name.toLowerCase().includes(q));
	}

	async open() {
		this.visible = true;
		this.query = '';
		this.error = null;
		this.selectedIndex = 0;
		await this.refresh();
	}

	close() {
		this.visible = false;
		this.query = '';
		this.error = null;
		this.renamingId = null;
		this.renameDraft = '';
	}

	async refresh() {
		this.loading = true;
		try {
			this.entries = await ipc.workspaces.catalog();
			if (this.selectedIndex >= this.entries.length) {
				this.selectedIndex = Math.max(0, this.entries.length - 1);
			}
		} catch (err) {
			this.error = formatError(err);
		} finally {
			this.loading = false;
		}
	}

	moveSelection(delta: number) {
		const list = this.filtered;
		if (list.length === 0) {
			return;
		}
		const next = (this.selectedIndex + delta + list.length) % list.length;
		this.selectedIndex = next;
	}

	async activate(meta: WorkspaceMeta) {
		try {
			await ipc.window.open(meta.id);
			this.close();
		} catch (err) {
			this.error = formatError(err);
		}
	}

	async forget(meta: WorkspaceMeta) {
		if (meta.id === currentWorkspaceId()) {
			this.error = 'Switch to a different workspace before forgetting this one.';
			return;
		}
		try {
			await ipc.workspaces.delete(meta.id);
			await this.refresh();
		} catch (err) {
			this.error = formatError(err);
		}
	}

	/** Put `meta`'s row into inline-rename mode. The filter input
	 * stays untouched — the rename draft is a separate field so
	 * starting a rename doesn't clobber what the user typed to
	 * find the row. */
	startRename(meta: WorkspaceMeta) {
		this.renamingId = meta.id;
		this.renameDraft = meta.name;
		this.error = null;
	}

	cancelRename() {
		this.renamingId = null;
		this.renameDraft = '';
	}

	/** Commit the inline rename. Renaming the process's own
	 * workspace re-syncs `workspace.workspaceName` so the title
	 * bar (`App.svelte`'s `$effect`) repaints immediately —
	 * sibling processes pick the new name up from the catalog on
	 * their next launch. */
	async commitRename(meta: WorkspaceMeta) {
		const name = this.renameDraft.trim();
		if (name.length === 0) {
			this.error = 'Workspace name must not be empty.';
			return;
		}
		try {
			const updated = await ipc.workspaces.rename(meta.id, name);
			this.entries = this.entries.map((m) => (m.id === updated.id ? updated : m));
			if (updated.id === currentWorkspaceId()) {
				workspace.workspaceName = updated.name;
			}
			this.cancelRename();
		} catch (err) {
			this.error = formatError(err);
		}
	}

	/** Update `meta.id`'s badge colour. Pass `''` to reset to the
	 * deterministic default. Optimistic update so the swatch
	 * snaps to the new colour instantly; on failure we surface
	 * the error and roll back from the server. */
	async setColor(meta: WorkspaceMeta, color: string) {
		const previous = meta.color ?? null;
		const next = color.trim().length === 0 ? null : color.trim();
		this.entries = this.entries.map((m) => (m.id === meta.id ? { ...m, color: next } : m));
		// Re-paint the colour scheme live when the recoloured
		// workspace is the one this process owns — same optimistic
		// semantics as the swatch above.
		if (meta.id === currentWorkspaceId()) {
			applyWorkspaceScheme(meta.id, next);
		}
		try {
			await ipc.workspaces.setColor(meta.id, next ?? '');
		} catch (err) {
			if (meta.id === currentWorkspaceId()) {
				applyWorkspaceScheme(meta.id, previous);
			}
			this.error = formatError(err);
			this.entries = this.entries.map((m) => (m.id === meta.id ? { ...m, color: previous } : m));
		}
	}
}

/** Mirror of `window_icon::workspace_colour` in the Rust side:
 * deterministic FNV-1a hash → HSL hue, saturation 58 %,
 * lightness 52 %. Used to paint the swatch when a workspace
 * doesn't have a user-chosen colour yet, so the picker preview
 * matches what the OS would show in alt-tab. */
export function defaultWorkspaceColor(workspaceId: string): string {
	const FNV_OFFSET = 0xcbf29ce484222325n;
	const FNV_PRIME = 0x100000001b3n;
	const MASK = 0xffffffffffffffffn;
	let h = FNV_OFFSET;
	for (let i = 0; i < workspaceId.length; i++) {
		h = (h ^ BigInt(workspaceId.charCodeAt(i) & 0xff)) & MASK;
		h = (h * FNV_PRIME) & MASK;
	}
	const hue = Number(h % 360n);
	return hslToHex(hue, 0.58, 0.52);
}

function hslToHex(h: number, s: number, l: number): string {
	const c = (1 - Math.abs(2 * l - 1)) * s;
	const hp = h / 60;
	const x = c * (1 - Math.abs((hp % 2) - 1));
	let r1 = 0;
	let g1 = 0;
	let b1 = 0;
	if (hp < 1) {
		[r1, g1, b1] = [c, x, 0];
	} else if (hp < 2) {
		[r1, g1, b1] = [x, c, 0];
	} else if (hp < 3) {
		[r1, g1, b1] = [0, c, x];
	} else if (hp < 4) {
		[r1, g1, b1] = [0, x, c];
	} else if (hp < 5) {
		[r1, g1, b1] = [x, 0, c];
	} else {
		[r1, g1, b1] = [c, 0, x];
	}
	const m = l - c / 2;
	const toHex = (v: number) =>
		Math.max(0, Math.min(255, Math.round((v + m) * 255)))
			.toString(16)
			.padStart(2, '0');
	return `#${toHex(r1)}${toHex(g1)}${toHex(b1)}`;
}

export const workspacePicker = new WorkspacePickerStore();
