// "Open remote workspace…" modal (ADR 0095): adds a catalog entry for
// a workspace on an SSH-reachable server and opens its window.

import { ipc } from './ipc';
import { formatError } from './protocol';

class RemoteConnectStore {
	visible = $state(false);
	host = $state('');
	workspace = $state('');
	name = $state('');
	error = $state<string | null>(null);
	busy = $state(false);

	open() {
		this.visible = true;
		this.host = '';
		this.workspace = '';
		this.name = '';
		this.error = null;
		this.busy = false;
	}

	close() {
		this.visible = false;
		this.error = null;
		this.busy = false;
	}

	async submit(): Promise<void> {
		const host = this.host.trim();
		const workspace = this.workspace.trim();
		if (host.length === 0 || workspace.length === 0) {
			this.error = 'Enter the SSH host and the workspace name on the server.';
			return;
		}
		const name = this.name.trim() || `${workspace} ${host}`;
		this.busy = true;
		this.error = null;
		try {
			const meta = await ipc.workspaces.addRemote(name, host, workspace);
			await ipc.window.open(meta.id);
			this.close();
		} catch (err) {
			this.error = formatError(err);
			this.busy = false;
		}
	}
}

export const remoteConnect = new RemoteConnectStore();
