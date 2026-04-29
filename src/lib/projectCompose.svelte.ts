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

/** Mutating actions a folder bar can dispatch. */
export type ProjectComposeAction = 'up' | 'pause' | 'resume' | 'rebuild' | 'down';

class ProjectComposeStateStore {
	/** Per-folder snapshot cache. Key is the folder's absolute path. */
	#snapshots = new SvelteMap<string, ProjectComposeStatus>();

	/** Per-folder in-flight tracker — at most one mutation per folder. */
	#inFlight = new SvelteMap<string, ProjectComposeAction>();

	/** Per-folder last-error string. Cleared on successful mutation. */
	#errors = new SvelteMap<string, string>();

	/** Per-folder popover open flag — folder bars share the panel
	 * implementation but each tracks its own visibility. */
	#openPanel = new SvelteMap<string, boolean>();

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
		return this.#inFlight.get(folderPath);
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
			}
		}
		const next = !wasOpen;
		this.#openPanel.set(folderPath, next);
		if (next) {
			void this.refresh(folderPath);
		}
	}

	closePanel(folderPath: string): void {
		this.#openPanel.set(folderPath, false);
	}

	/** Drop a folder's cached state — called when it leaves the workspace. */
	forget(folderPath: string): void {
		this.#snapshots.delete(folderPath);
		this.#inFlight.delete(folderPath);
		this.#errors.delete(folderPath);
		this.#openPanel.delete(folderPath);
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
		await this.#run(folderPath, 'up', () => ipc.projectCompose.up(folderPath));
	}

	async pause(folderPath: string): Promise<void> {
		await this.#run(folderPath, 'pause', () => ipc.projectCompose.pause(folderPath));
	}

	async resume(folderPath: string): Promise<void> {
		await this.#run(folderPath, 'resume', () => ipc.projectCompose.resume(folderPath));
	}

	async rebuild(folderPath: string): Promise<void> {
		await this.#run(folderPath, 'rebuild', () => ipc.projectCompose.rebuild(folderPath));
	}

	async down(folderPath: string): Promise<void> {
		await this.#run(folderPath, 'down', () => ipc.projectCompose.down(folderPath));
	}

	async #run(folderPath: string, label: ProjectComposeAction, op: () => Promise<ProjectComposeStatus>): Promise<void> {
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
