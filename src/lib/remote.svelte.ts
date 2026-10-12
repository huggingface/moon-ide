// Connection state of a window fronting a remote workspace (ADR 0095).
// Hydration waits for the first connection; any later reconnect
// reloads the page, since events emitted while disconnected or
// detached are gone and every store would otherwise be stale.

import { ipc } from './ipc';
import type { AppInfo, RemoteStatus, RemoteTarget } from './protocol';
import { subscribe } from './transport';

class RemoteStore {
	target = $state<RemoteTarget | null>(null);
	status = $state<RemoteStatus | null>(null);
	reattaching = $state(false);
	#everConnected = false;
	#sawEvent = false;
	#connectedWaiters: (() => void)[] = [];

	async init(info: AppInfo): Promise<void> {
		if (info.remote === null) {
			return;
		}
		this.target = info.remote;
		await subscribe('remote:status', (status: RemoteStatus) => {
			this.#sawEvent = true;
			this.#apply(status);
		});
		const current = await ipc.remote.status();
		// A pushed event is newer than a status read issued before it.
		if (current !== null && !this.#sawEvent) {
			this.#apply(current);
		}
	}

	waitUntilConnected(): Promise<void> {
		if (this.target === null || this.status?.state === 'connected') {
			return Promise.resolve();
		}
		return new Promise((resolve) => this.#connectedWaiters.push(resolve));
	}

	async reattach(): Promise<void> {
		this.reattaching = true;
		try {
			await ipc.remote.reattach();
		} finally {
			this.reattaching = false;
		}
	}

	#apply(status: RemoteStatus) {
		const wasConnected = this.status?.state === 'connected';
		this.status = status;
		if (status.state !== 'connected' || wasConnected) {
			return;
		}
		if (this.#everConnected) {
			location.reload();
			return;
		}
		this.#everConnected = true;
		for (const resolve of this.#connectedWaiters.splice(0)) {
			resolve();
		}
	}
}

export const remote = new RemoteStore();
