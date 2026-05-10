// Reactive state for the "New Workspace" modal. Bound to the
// global `Ctrl+Shift+N` shortcut in `App.svelte`. Submit is
// **create-or-switch**: if the typed name slugifies to an
// existing catalog entry, we just open / focus that workspace
// instead of erroring. New entries get a fresh `moon-ide
// --workspace <slug>` child; existing ones are focused if a
// sibling process owns them, or re-opened if not.
//
// In preboot mode the calling process exits after the child
// spawn (preboot's only job is to collect a name and hand off).

import { ipc } from './ipc';
import { formatError } from './protocol';
import { currentAppInfo } from './workspace-id';

class WorkspaceCreateStore {
	visible = $state(false);
	name = $state('');
	error = $state<string | null>(null);
	busy = $state(false);

	open() {
		this.visible = true;
		this.name = '';
		this.error = null;
		this.busy = false;
	}

	close() {
		this.visible = false;
		this.name = '';
		this.error = null;
		this.busy = false;
	}

	async submit(): Promise<boolean> {
		const trimmed = this.name.trim();
		if (trimmed.length === 0) {
			this.error = 'Pick a name for the workspace.';
			return false;
		}
		this.busy = true;
		this.error = null;
		try {
			// Empty slug = let the backend slugify the name.
			const meta = await ipc.workspaces.create('', trimmed);
			await ipc.window.open(meta.id);
			if (currentAppInfo().mode === 'preboot') {
				// Preboot: exit so the user is left with just
				// the freshly-spawned workspace process.
				await ipc.window.close();
				return true;
			}
			this.close();
			return true;
		} catch (err) {
			this.error = formatError(err);
			this.busy = false;
			return false;
		}
	}
}

export const workspaceCreate = new WorkspaceCreateStore();
