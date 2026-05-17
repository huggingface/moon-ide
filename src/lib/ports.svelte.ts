//! Reactive state for workspace port forwarding.
//!
//! Mirrors `container.svelte.ts` — module-level singleton, lazy
//! `wireRuntime` for the `ports:state` Tauri event, hydrate /
//! refresh entry points called from `WorkspaceState` and the
//! Ports panel.
//!
//! Storage truth lives in `session.json` on disk; this store is
//! a cache that round-trips through the IPC layer. Mutations
//! always go through `commit` (replace-the-whole-set), which
//! mirrors the backend command shape and avoids the partial-edit
//! consistency problems an "add port" / "remove port" pair would
//! introduce.
//!
//! See [specs/containers.md](../../specs/containers.md) §
//! "Network and port forwarding".

import { listen, type UnlistenFn } from '@tauri-apps/api/event';

import { ipc } from './ipc';
import {
	formatError,
	type ForwardedPort,
	type ForwardedPortHealth,
	type ForwardedPortStatus,
	type PortsApplyResult,
} from './protocol';

/** Tauri event name — must match `PORTS_STATE_EVENT` in
 *  `src-tauri/src/commands/ports.rs`. */
const PORTS_STATE_EVENT = 'ports:state';

class PortsStore {
	/** Latest status snapshot (one entry per declared forward).
	 *  Empty until `refresh()` lands. */
	status = $state<ForwardedPortStatus[]>([]);

	/** True while a mutating command is in flight. The picker
	 *  disables its inputs to avoid stomping a pending apply. */
	busy = $state(false);

	/** Most recent error from `commit` / `reapply`. Cleared on
	 *  the next successful run. */
	lastError = $state<string | null>(null);

	/** Conflicts surfaced by the most recent `commit`. The
	 *  picker reads this to render a "host port busy" hint
	 *  inline; cleared when the user retries. */
	conflicts = $state<ForwardedPort[]>([]);

	#unlisten: UnlistenFn[] = [];
	#runtimeWired = false;

	get forwards(): readonly ForwardedPort[] {
		return this.status.map((s) => s.forward);
	}

	healthFor(hostPort: number): ForwardedPortHealth | null {
		return this.status.find((s) => s.forward.host_port === hostPort)?.health ?? null;
	}

	/** Bind the `ports:state` Tauri event. Idempotent — safe to
	 *  call from `App.svelte`'s onMount even with HMR. */
	async wireRuntime(): Promise<void> {
		if (this.#runtimeWired) {
			return;
		}
		this.#runtimeWired = true;
		try {
			const unlisten = await listen<ForwardedPortStatus[]>(PORTS_STATE_EVENT, (event) => {
				this.status = event.payload;
			});
			this.#unlisten.push(unlisten);
		} catch {
			// Event-bus bind failed. The panel still works — every
			// mutating command returns the latest status — we just
			// miss out on cross-command updates.
		}
	}

	/** Fetch latest declared forwards + per-port live state.
	 *  Called on workspace change and when the Ports tab opens. */
	async refresh(): Promise<void> {
		try {
			this.status = await ipc.ports.status();
			this.lastError = null;
		} catch (err) {
			this.lastError = formatError(err);
		}
	}

	/** Replace the declared forward set. Persists to
	 *  `session.json` and reapplies the proxy sidecar. The
	 *  `conflicts` field on the result drives the per-row "host
	 *  port busy" hint in the picker. */
	async commit(forwards: ForwardedPort[]): Promise<PortsApplyResult> {
		this.busy = true;
		try {
			const result = await ipc.ports.set(forwards);
			this.conflicts = result.conflicts;
			this.lastError = null;
			await this.refresh();
			return result;
		} catch (err) {
			this.lastError = formatError(err);
			throw err;
		} finally {
			this.busy = false;
		}
	}

	/** Re-create the proxy sidecar for the persisted set. Used
	 *  by `WorkspaceState` after the workspace shell comes up
	 *  (a fresh `dev` has no sidecar yet, but the user's
	 *  forwards are still on disk). No-op when nothing is
	 *  persisted; doesn't surface errors to the UI — the user
	 *  didn't ask for this, it's reconciliation. */
	async reapply(): Promise<void> {
		try {
			const result = await ipc.ports.reapply();
			this.conflicts = result.conflicts;
			await this.refresh();
		} catch (err) {
			// Logged for debugging; not surfaced — the user can
			// retry by editing forwards in the panel.
			// eslint-disable-next-line no-console
			console.warn('ports.reapply failed:', formatError(err));
		}
	}

	dispose(): void {
		for (const fn of this.#unlisten) {
			fn();
		}
		this.#unlisten = [];
		this.#runtimeWired = false;
		this.status = [];
		this.conflicts = [];
		this.lastError = null;
	}
}

export const ports = new PortsStore();
