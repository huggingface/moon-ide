//! Reactive state for the container status pip.
//!
//! Mirrors the structure of `slack.svelte.ts` — module-level
//! singleton, `wireRuntime` for Tauri event subscriptions,
//! `hydrate`/`refresh` entry points called from `WorkspaceState`
//! and `App.svelte`.
//!
//! Phase 2.0 surface: poll on demand (workspace change + after
//! every mutating action) and react to `container:state` events.
//! No periodic poller — if the user runs `docker compose down`
//! from a terminal, the IDE state stays stale until the next
//! mutating action or window focus refresh. That's an explicit
//! 2.0 trade-off; 2.2 adds a docker-events watcher on the Rust
//! side and the staleness window collapses.

import { listen, type UnlistenFn } from '@tauri-apps/api/event';

import { ipc } from './ipc';
import { formatError, type ContainerStateChange, type ContainerStatus } from './protocol';

/** Tauri event name — must match `CONTAINER_STATE_EVENT` in `src-tauri/src/commands/container.rs`. */
const CONTAINER_STATE_EVENT = 'container:state';

/** Identifies which mutating command is currently in flight. */
export type ContainerInFlight = null | 'setup' | 'pause' | 'resume' | 'rebuild' | 'teardown';

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
			// won't see updates from other windows. No actionable
			// surface to flash for.
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
	 * Reset to the "no workspace" state. Called when the workspace
	 * changes so the pip never shows a stale snapshot from the
	 * previous workspace mid-transition.
	 */
	resetForWorkspaceSwitch() {
		this.status = null;
		this.lastError = null;
		this.composePreview = null;
		this.previewVisible = false;
		this.previewError = null;
		// Don't clear `inFlight` — the previous workspace's command
		// has its own await chain; let it complete and clean itself
		// up. (In practice the user can't switch workspaces while a
		// container command is in flight because the buttons are
		// disabled, but the invariant is cheap to keep.)
	}

	/**
	 * Pull the latest status from the backend. Called on workspace
	 * change, on panel open, and as the after-step of every mutating
	 * command. No-op if there's no active workspace — the backend
	 * would error with `InvalidArgument`, which we shouldn't surface
	 * as a user-visible failure.
	 */
	async refresh(): Promise<void> {
		try {
			this.status = await ipc.container.status();
			this.lastError = null;
		} catch (err) {
			// `InvalidArgument: no active workspace` happens before
			// any workspace is open — that's not a real error, just
			// "we don't have a status to show yet".
			if (formatError(err).includes('no active workspace')) {
				this.status = null;
				return;
			}
			this.lastError = formatError(err);
		}
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

	async teardown(): Promise<void> {
		await this.#run('teardown', () => ipc.container.teardown());
	}

	/**
	 * Fetch the would-be `.moon/compose.yaml` and cache it for the
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
