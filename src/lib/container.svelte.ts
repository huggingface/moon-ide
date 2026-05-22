//! Reactive state for the container status pip.
//!
//! Mirrors the structure of `slack.svelte.ts` — module-level
//! singleton, `wireRuntime` for Tauri event subscriptions,
//! `hydrate`/`refresh` entry points called from `WorkspaceState`
//! and `App.svelte`.
//!
//! Phase 2.0 surface: poll on demand (workspace change + after
//! every mutating action, plus a window-focus refresh wired in
//! `wireRuntime`) and react to `container:state` events. No
//! periodic poller — if the user runs `docker compose down`
//! from a terminal while the IDE has focus, the IDE state stays
//! stale until the next mutating action or focus blur/regain.
//! That's an explicit 2.0 trade-off; 2.2 adds a docker-events
//! watcher on the Rust side and the staleness window collapses.

import { listen, type UnlistenFn } from '@tauri-apps/api/event';

import { ipc } from './ipc';
import { formatError, type ContainerStateChange, type ContainerStatus } from './protocol';

/** Tauri event name — must match `CONTAINER_STATE_EVENT` in `src-tauri/src/commands/container.rs`. */
const CONTAINER_STATE_EVENT = 'container:state';

/** Identifies which mutating command is currently in flight. */
export type ContainerInFlight = null | 'setup' | 'pause' | 'resume' | 'rebuild' | 'stop' | 'teardown' | 'sync-folders';

class ContainerPanelState {
	/** Last known status. `null` before the first `refresh()`. */
	status = $state<ContainerStatus | null>(null);

	/** Whether the popover is mounted (anchored to the status-bar pip). */
	panelOpen = $state(false);

	/** Currently in-flight mutating command, if any. */
	inFlight = $state<ContainerInFlight>(null);

	/** Most recent error from a lifecycle command. Cleared on success. */
	lastError = $state<string | null>(null);

	/** Cached compose preview from `container_render_compose`. Lazily
	 * fetched the first time the user expands "Inspect compose.yaml". */
	composePreview = $state<string | null>(null);
	previewVisible = $state(false);
	previewError = $state<string | null>(null);

	#unlisten: UnlistenFn[] = [];
	#runtimeWired = false;
	/** In-flight `refresh()` promise. Callers that need a truthful
	 *  status answer (the terminal-spawn helpers in particular) await
	 *  this so they don't race the startup refresh and silently fall
	 *  back to host. `null` between refreshes. */
	#inFlightRefresh: Promise<void> | null = null;

	/** True iff the IDE has a workspace open AND status has been loaded
	 * at least once. The status-bar pip is hidden until then to avoid
	 * a brief "absent" flash on every launch. */
	get visible(): boolean {
		return this.status !== null;
	}

	/** Convenience accessor for the high-level state. */
	get state(): ContainerStatus['state'] | null {
		return this.status?.state ?? null;
	}

	/** Resolve once any in-flight `refresh()` has settled. Used by
	 *  terminal-spawn helpers so an early click during the startup
	 *  probe doesn't see `status === null` and pick host by default.
	 *  Resolves immediately when no refresh is pending. */
	async awaitRefreshed(): Promise<void> {
		if (this.#inFlightRefresh) {
			await this.#inFlightRefresh;
		}
	}

	/**
	 * Bind the `container:state` Tauri event. Idempotent — safe to
	 * call from `App.svelte`'s onMount even with HMR.
	 */
	async wireRuntime(): Promise<void> {
		if (this.#runtimeWired) {
			return;
		}
		this.#runtimeWired = true;
		try {
			const unlisten = await listen<ContainerStateChange>(CONTAINER_STATE_EVENT, (event) => {
				this.status = event.payload.status;
			});
			this.#unlisten.push(unlisten);
		} catch {
			// Event-bus bind failed. The pip still works — every
			// command response carries the new status, so we just
			// miss out on updates from background events (docker
			// daemon state changes etc). No actionable surface to
			// flash for.
		}
		// Re-probe on window focus so external `docker compose`
		// activity (the user ran `up`/`down` from a host terminal
		// while moon-ide was in the background) reconciles without
		// needing a pip click. Cheap — one `docker compose ps` per
		// focus event.
		if (typeof window !== 'undefined') {
			window.addEventListener('focus', () => {
				void this.refresh();
			});
		}
	}

	togglePanel() {
		this.panelOpen = !this.panelOpen;
		if (this.panelOpen) {
			void this.refresh();
		}
	}

	closePanel() {
		this.panelOpen = false;
	}

	/**
	 * Drop the cached compose preview. Called when the bound-folder
	 * set changes so a re-open of the "Inspect" panel re-renders
	 * with the new mounts. The high-level status doesn't get
	 * cleared — the compose project survives folder add/remove,
	 * so the pip should continue to reflect whatever containers
	 * are actually up.
	 */
	invalidateComposePreview() {
		this.composePreview = null;
		if (this.previewVisible) {
			void this.loadComposePreview();
		}
	}

	/**
	 * Pull the latest status from the backend. Called on workspace
	 * change, on panel open, and as the after-step of every mutating
	 * command.
	 */
	async refresh(): Promise<void> {
		// Coalesce concurrent callers onto the same in-flight probe
		// so `awaitRefreshed()` sees a single source of truth and a
		// follow-up `refresh()` from a click handler doesn't double
		// up on the daemon round-trip.
		if (this.#inFlightRefresh) {
			return this.#inFlightRefresh;
		}
		const run = (async () => {
			try {
				this.status = await ipc.container.status();
				this.lastError = null;
			} catch (err) {
				this.lastError = formatError(err);
			} finally {
				this.#inFlightRefresh = null;
			}
		})();
		this.#inFlightRefresh = run;
		return run;
	}

	async setup(): Promise<void> {
		await this.#run('setup', () => ipc.container.setup());
	}

	async pause(): Promise<void> {
		await this.#run('pause', () => ipc.container.pause());
	}

	async resume(): Promise<void> {
		await this.#run('resume', () => ipc.container.resume());
	}

	async rebuild(): Promise<void> {
		await this.#run('rebuild', () => ipc.container.rebuild());
	}

	async stop(): Promise<void> {
		await this.#run('stop', () => ipc.container.stop());
	}

	async teardown(): Promise<void> {
		await this.#run('teardown', () => ipc.container.teardown());
	}

	/**
	 * Re-emit `compose.yaml` from the current bound-folder set,
	 * and apply via `compose up -d --wait` if the project is
	 * already running. Called after `WorkspaceState.openLocal` /
	 * `removeFolder` succeeds. Cheap when the compose project
	 * isn't running — the backend just rewrites the file.
	 *
	 * Drops the cached compose preview either way so a follow-up
	 * "Inspect" reflects the new mounts.
	 */
	async syncBoundFolders(): Promise<void> {
		this.invalidateComposePreview();
		await this.#run('sync-folders', () => ipc.container.applyBoundFolders());
	}

	/**
	 * Fetch the would-be workspace `compose.yaml` and cache it for the
	 * preview panel. Cheap, no daemon round-trip — purely a render of
	 * the discovery + generator output.
	 */
	async loadComposePreview(): Promise<void> {
		this.previewError = null;
		try {
			this.composePreview = await ipc.container.renderCompose();
		} catch (err) {
			this.previewError = formatError(err);
		}
	}

	togglePreview() {
		this.previewVisible = !this.previewVisible;
		if (this.previewVisible && this.composePreview === null) {
			void this.loadComposePreview();
		}
	}

	async #run(label: ContainerInFlight, op: () => Promise<ContainerStatus>): Promise<void> {
		if (this.inFlight !== null) {
			return;
		}
		this.inFlight = label;
		this.lastError = null;
		try {
			const next = await op();
			this.status = next;
		} catch (err) {
			this.lastError = formatError(err);
			// Still refresh — the command may have partially
			// applied (e.g. `up -d --wait` failed at the wait step
			// but containers exist), and the user wants to see
			// whatever truth the daemon reports.
			await this.refresh();
		} finally {
			this.inFlight = null;
		}
	}
}

export const container = new ContainerPanelState();

/**
 * Human-readable label for each state. Kept here (not in the
 * StatusBar component) so the command palette and any future
 * affordance can reach for the same wording.
 */
export function containerStateLabel(state: ContainerStatus['state'] | null): string {
	const labels: Record<NonNullable<ContainerStatus['state']>, string> = {
		absent: 'not set up',
		creating: 'setting up…',
		running: 'running',
		paused: 'paused',
		stopped: 'stopped',
		failed: 'failed',
	};
	if (state === null) {
		return 'unknown';
	}
	return labels[state];
}
