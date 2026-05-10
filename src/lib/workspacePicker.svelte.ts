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

import { ipc } from './ipc';
import { formatError } from './protocol';
import type { WorkspaceMeta } from './protocol';
import { currentWorkspaceId } from './workspace-id';

class WorkspacePickerStore {
	visible = $state(false);
	query = $state('');
	entries = $state<WorkspaceMeta[]>([]);
	error = $state<string | null>(null);
	loading = $state(false);
	selectedIndex = $state(0);

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
}

export const workspacePicker = new WorkspacePickerStore();
