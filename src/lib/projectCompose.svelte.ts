//! Reactive state for per-folder compose projects.
//!
//! Sibling of `container.svelte.ts`: same shape (status cache,
//! in-flight tracker, error surface, Tauri event subscription) but
//! keyed on `folder_path` so each bound folder tracks its own
//! compose project independently. The folder bar reads
//! `projectCompose.snapshotFor(folder)` to decide whether to show
//! the indicator and what color to render it.
//!
//! Why separate from `container.svelte.ts`
//! ---------------------------------------
//!
//! The two surfaces represent *conceptually different* things to
//! the user:
//!
//! - The workspace shell is the IDE's own container. Single
//!   instance, IDE-managed, expected to be up. The status pip
//!   in the bottom bar reflects this.
//! - Project services are per-folder, user-driven. Many bound
//!   folders, often started/stopped on demand, allowed to be
//!   absent or failed without breaking the IDE.
//!
//! Mixing them under one store would make the lookup-by-folder
//! ergonomics awkward and mask the workspace shell's much-simpler
//! lifecycle behind a Map indirection.

import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import { SvelteMap } from 'svelte/reactivity';

import { ipc } from './ipc';
import { formatError, type ProjectComposeStateChange, type ProjectComposeStatus } from './protocol';

/** Tauri event name — must match `PROJECT_COMPOSE_STATE_EVENT` in `src-tauri/src/commands/project_compose.rs`. */
const PROJECT_COMPOSE_STATE_EVENT = 'project_compose:state';

/** Project-level compose lifecycle verbs the folder bar can fire. */
export type ProjectComposeProjectAction = 'up' | 'pause' | 'resume' | 'rebuild' | 'stop' | 'down';

/** Per-service compose lifecycle verbs fired from a service row. */
export type ProjectComposeServiceAction = 'service-start' | 'service-stop' | 'service-restart';

export type ProjectComposeAction = ProjectComposeProjectAction | ProjectComposeServiceAction;

/** What's currently in flight against a folder, if anything.
 * Keeps a single per-folder lock so a service-level command and
 * a project-level command can't race on the same compose project
 * (e.g. user clicks Restart-gitaly while `up -d --wait` is still
 * running) — but `service` lets the UI say _which_ service is
 * being acted on, so the right row gets the spinner. */
export type ProjectComposeInFlight =
	| { action: ProjectComposeProjectAction; service?: undefined }
	| { action: ProjectComposeServiceAction; service: string };

/**
 * How often the popover re-polls `project_compose_status` while
 * it's open. The Tauri event only fires when a *mutation*
 * resolves, but `compose up -d --wait` can block for minutes —
 * without polling, the user's open popover would freeze on the
 * pre-`up` snapshot until the entire command returns. 2 s is
 * cheap (a single `docker compose ps` per folder) and keeps the
 * "which service are we waiting on" display live.
 */
const PANEL_POLL_INTERVAL_MS = 2000;

class ProjectComposeStateStore {
	/** Per-folder snapshot cache. Key is the folder's absolute path. */
	#snapshots = new SvelteMap<string, ProjectComposeStatus>();

	/** Per-folder in-flight tracker — at most one mutation per folder. */
	#inFlight = new SvelteMap<string, ProjectComposeInFlight>();

	/** Per-folder last-error string. Cleared on successful mutation. */
	#errors = new SvelteMap<string, string>();

	/** Per-folder popover open flag — folder bars share the panel
	 * implementation but each tracks its own visibility. */
	#openPanel = new SvelteMap<string, boolean>();

	/** Per-folder live-polling handles. Set while the popover is
	 * open so service-level state updates without waiting for the
	 * mutation to resolve. */
	#pollers = new Map<string, ReturnType<typeof setInterval>>();

	#unlisten: UnlistenFn[] = [];
	#runtimeWired = false;

	/**
	 * Bind the `project_compose:state` Tauri event. Idempotent —
	 * safe across HMR remounts. Updates only the entry for the
	 * folder named in the event payload.
	 */
	async wireRuntime(): Promise<void> {
		if (this.#runtimeWired) {
			return;
		}
		this.#runtimeWired = true;
		try {
			const unlisten = await listen<ProjectComposeStateChange>(PROJECT_COMPOSE_STATE_EVENT, (event) => {
				this.#snapshots.set(event.payload.folder_path, event.payload.project);
			});
			this.#unlisten.push(unlisten);
		} catch {
			// Event-bus bind failed. Per-folder mutations still
			// hand back fresh snapshots in their command result;
			// we just won't see updates from other windows.
		}
	}

	/** Reactive lookup. `undefined` until the folder's first poll. */
	snapshotFor(folderPath: string): ProjectComposeStatus | undefined {
		return this.#snapshots.get(folderPath);
	}

	inFlightFor(folderPath: string): ProjectComposeAction | undefined {
		return this.#inFlight.get(folderPath)?.action;
	}

	/** When a service-level mutation is in flight, name the
	 * service it targets — the popover uses this to spotlight
	 * the row. `undefined` for project-level actions. */
	inFlightServiceFor(folderPath: string): string | undefined {
		return this.#inFlight.get(folderPath)?.service;
	}

	errorFor(folderPath: string): string | undefined {
		return this.#errors.get(folderPath);
	}

	isPanelOpen(folderPath: string): boolean {
		return this.#openPanel.get(folderPath) ?? false;
	}

	togglePanel(folderPath: string): void {
		const wasOpen = this.isPanelOpen(folderPath);
		// Single-panel UX: opening folder X's popover dismisses
		// any other folder's. There's no way to view two folder
		// popovers at once and leaving stale ones around behind
		// the active row would be confusing.
		for (const key of this.#openPanel.keys()) {
			if (key !== folderPath) {
				this.#openPanel.set(key, false);
				this.#stopPolling(key);
			}
		}
		const next = !wasOpen;
		this.#openPanel.set(folderPath, next);
		if (next) {
			void this.refresh(folderPath);
			this.#startPolling(folderPath);
		} else {
			this.#stopPolling(folderPath);
		}
	}

	closePanel(folderPath: string): void {
		this.#openPanel.set(folderPath, false);
		this.#stopPolling(folderPath);
	}

	/** Drop a folder's cached state — called when it leaves the workspace. */
	forget(folderPath: string): void {
		this.#snapshots.delete(folderPath);
		this.#inFlight.delete(folderPath);
		this.#errors.delete(folderPath);
		this.#openPanel.delete(folderPath);
		this.#stopPolling(folderPath);
	}

	#startPolling(folderPath: string): void {
		if (this.#pollers.has(folderPath)) {
			return;
		}
		const handle = setInterval(() => {
			void this.refresh(folderPath);
		}, PANEL_POLL_INTERVAL_MS);
		this.#pollers.set(folderPath, handle);
	}

	#stopPolling(folderPath: string): void {
		const handle = this.#pollers.get(folderPath);
		if (handle === undefined) {
			return;
		}
		clearInterval(handle);
		this.#pollers.delete(folderPath);
	}

	/**
	 * Pure read. Called when a folder bar mounts (so its indicator
	 * has something to render) and after the active folder
	 * switches.
	 */
	async refresh(folderPath: string): Promise<void> {
		try {
			const snap = await ipc.projectCompose.status(folderPath);
			this.#snapshots.set(folderPath, snap);
			this.#errors.delete(folderPath);
		} catch (err) {
			this.#errors.set(folderPath, formatError(err));
		}
	}

	/**
	 * Refresh every bound folder's snapshot. Cheap when most
	 * folders have no compose file (the backend short-circuits
	 * to `Absent`); used after the workspace hydrates so all
	 * folder bars paint with real data on first frame.
	 */
	async refreshAll(folderPaths: string[]): Promise<void> {
		await Promise.all(folderPaths.map((p) => this.refresh(p)));
	}

	async up(folderPath: string): Promise<void> {
		await this.#run(folderPath, { action: 'up' }, () => ipc.projectCompose.up(folderPath));
	}

	async pause(folderPath: string): Promise<void> {
		await this.#run(folderPath, { action: 'pause' }, () => ipc.projectCompose.pause(folderPath));
	}

	async resume(folderPath: string): Promise<void> {
		await this.#run(folderPath, { action: 'resume' }, () => ipc.projectCompose.resume(folderPath));
	}

	async rebuild(folderPath: string): Promise<void> {
		await this.#run(folderPath, { action: 'rebuild' }, () => ipc.projectCompose.rebuild(folderPath));
	}

	async stop(folderPath: string): Promise<void> {
		await this.#run(folderPath, { action: 'stop' }, () => ipc.projectCompose.stop(folderPath));
	}

	async down(folderPath: string): Promise<void> {
		await this.#run(folderPath, { action: 'down' }, () => ipc.projectCompose.down(folderPath));
	}

	async startService(folderPath: string, service: string): Promise<void> {
		await this.#run(folderPath, { action: 'service-start', service }, () =>
			ipc.projectCompose.serviceStart(folderPath, service),
		);
	}

	async stopService(folderPath: string, service: string): Promise<void> {
		await this.#run(folderPath, { action: 'service-stop', service }, () =>
			ipc.projectCompose.serviceStop(folderPath, service),
		);
	}

	async restartService(folderPath: string, service: string): Promise<void> {
		await this.#run(folderPath, { action: 'service-restart', service }, () =>
			ipc.projectCompose.serviceRestart(folderPath, service),
		);
	}

	async #run(
		folderPath: string,
		label: ProjectComposeInFlight,
		op: () => Promise<ProjectComposeStatus>,
	): Promise<void> {
		if (this.#inFlight.get(folderPath)) {
			return;
		}
		this.#inFlight.set(folderPath, label);
		this.#errors.delete(folderPath);
		try {
			const next = await op();
			this.#snapshots.set(folderPath, next);
		} catch (err) {
			this.#errors.set(folderPath, formatError(err));
			// Mirror the workspace shell store: even on failure,
			// re-poll so partial-apply outcomes (e.g. some
			// services up, others failed) reach the UI.
			await this.refresh(folderPath);
		} finally {
			this.#inFlight.delete(folderPath);
		}
	}
}

export const projectCompose = new ProjectComposeStateStore();

/** Human-readable label for the per-folder pip glyph. Mirrors
 * `containerStateLabel` in `container.svelte.ts` but with
 * project-services wording (the user thinks of these as
 * "services", not the workspace's own shell). */
export function projectComposeStateLabel(snap: ProjectComposeStatus | undefined): string {
	if (!snap || snap.compose_file === null) {
		return 'no compose';
	}
	switch (snap.status.state) {
		case 'absent':
			return 'not running';
		case 'creating':
			return 'starting…';
		case 'running':
			return 'running';
		case 'paused':
			return 'paused';
		case 'stopped':
			return 'stopped';
		case 'failed':
			return 'failed';
		default:
			return 'unknown';
	}
}
