//! Reactive store for PTY-backed terminal sessions.
//!
//! One [`TerminalSession`] per open terminal tab. The Tauri side
//! allocates the PTY and emits `terminal:output` chunks +
//! `terminal:closed` once on exit; we forward output bytes to
//! the matching xterm.js instance and react to the close per its
//! [`TerminalCloseReason`]:
//!
//! - **Shell exits** (`shell_exited`, `container_shell_exited`) —
//!   the user's own Ctrl+D / `exit`, or a command that finished.
//!   The tab closes itself; a shell that ends is done.
//! - **Container losses** (`container_stopped`, `container_not_running`)
//!   — the environment went away (user Stop / Recreate, or a
//!   `docker exec` refusal while the container was still booting
//!   after an IDE relaunch). The tab stays with a banner offering
//!   to respawn the shell once the container is back.
//!
//! Persistence
//! -----------
//!
//! Terminal *tabs* persist across IDE launches (unlike log tabs —
//! see `bottomPanel.svelte.ts`): not the PTY, which dies with the
//! IDE, but the recipe — target, owning folder, and the
//! shell-history line the terminal last ran. `serialisePersisted`
//! snapshots the list into `AppState.bottom_panel.terminals`;
//! `hydratePersisted` + `restoreTerminals` replay it on launch by
//! spawning fresh shells and typing the recorded command into each
//! (which is also what seeds the new shell's history, so an
//! up-arrow afterwards keeps walking the old session). See ADR
//! 0009's "persistence" section.
//!
//! Why a writer registry instead of a buffer
//! -----------------------------------------
//!
//! `composeLogs` buffers lines in the store so the body
//! component can rerender on tab-switch from the store's
//! reactive state. xterm.js owns its own scrollback and ANSI
//! parser — replaying buffered bytes through it on every
//! mount would be expensive and fragile (ANSI state across
//! chunks). Instead, the active tab body registers an output
//! writer with the store; the store's single Tauri listener
//! dispatches incoming bytes to the right writer. When the
//! body unmounts (tab-switch), the writer un-registers and
//! pending output queues until it remounts.
//!
//! The bottom-panel chrome keeps every tab body mounted (just
//! display-hidden when inactive) so the xterm Terminal stays
//! alive across tab switches and keeps its scrollback. See
//! `BottomPanel.svelte`.

import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import { SvelteMap } from 'svelte/reactivity';

import { bottomPanel, type TerminalTab } from './bottomPanel.svelte';
import { container } from './container.svelte';
import { ipc } from './ipc';
import {
	formatError,
	type PersistedTerminal,
	type TerminalCloseReason,
	type TerminalClosed,
	type TerminalOpenRequest,
	type TerminalOutput,
	type TerminalTarget,
} from './protocol';

const OUTPUT_EVENT = 'terminal:output';
const CLOSED_EVENT = 'terminal:closed';

/** Per-tab session state surfaced reactively to the body. */
export type TerminalSession = {
	streamId: string;
	target: TerminalTarget;
	/** Bound folder (host path) the terminal was opened for, or
	 * `null` for a folder-less `$HOME` shell. Kept for the
	 * persistence snapshot. */
	folder: string | null;
	/** Set on `closed` for the container-loss reasons — the body
	 * swaps xterm for a "respawn when the container is back"
	 * banner. Shell-exit closes never reach this: the tab is
	 * gone by then. `null` while the session is live. */
	closedReason: TerminalCloseReason | null;
	/** Error returned by `terminal_open` itself. The tab still
	 * mounts so the message is visible. */
	openError: string | null;
};

type OutputWriter = (bytes: Uint8Array) => void;

/** Snapshot of the most recent non-empty selection across every
 *  open terminal pane. Updated by `TerminalTab` via xterm's
 *  `onSelectionChange` and read by App.svelte's Ctrl+L handler
 *  to attach the highlighted scrollback to the coder composer.
 *  Mirrors the editor's `activeSelection` shape: the *last
 *  meaningful selection wins*, since the user typically has at
 *  most one terminal in their attention at a time. */
export type TerminalSelectionSnapshot = {
	streamId: string;
	text: string;
	label: string;
};

class TerminalStore {
	#sessions = new SvelteMap<string, TerminalSession>();
	#writers = new Map<string, OutputWriter>();
	/** Buffer of output bytes that arrived while the body
	 * component wasn't mounted (e.g. tab opened, immediately
	 * switched away). Drained when a writer is registered. */
	#pending = new Map<string, Uint8Array[]>();
	/** The shell-history line recorded for each terminal — what
	 * one up-arrow in that shell would produce. Restart replays
	 * it into the fresh shell; persistence snapshots it for the
	 * next launch. `TerminalTab` refreshes the entry on every
	 * prompt-render escape it observes; entries simply go stale
	 * (never wrong) for shells whose prompt we don't recognise.
	 * Not reactive: nothing renders it. */
	#commands = new Map<string, string>();
	#unlisten: UnlistenFn[] = [];
	#runtimeWired = false;
	#onChange: (() => void) | null = null;
	/** Terminal tabs hydrated from disk at launch, waiting for
	 * `WorkspaceState.restoreAppState` to replay them once the
	 * container status / terminal event bus have settled. Kept
	 * out of the sessions map — they have no live PTY yet. */
	#restoring: PersistedTerminal[] = [];

	/** Most recent non-empty selection across all open terminal
	 * panes. `null` when every pane has its selection cleared.
	 * Reactive: the editor's "Add to Coder" hint pill in
	 * `EditorPane.svelte` shouldn't read this (it's for editor
	 * selections only); App.svelte's Ctrl+L handler reads it as
	 * a fallback when the editor has nothing selected. */
	activeSelection = $state<TerminalSelectionSnapshot | null>(null);

	/** Bound by `WorkspaceState.restoreAppState` alongside
	 * `bottomPanel.bindOnChange` so terminal open/close/restart
	 * lands in the same persist tick as panel chrome changes. */
	bindOnChange(handler: () => void): void {
		this.#onChange = handler;
	}

	#notify(): void {
		this.#onChange?.();
	}

	async wireRuntime(): Promise<void> {
		if (this.#runtimeWired) {
			return;
		}
		this.#runtimeWired = true;
		try {
			const onOutput = await listen<TerminalOutput>(OUTPUT_EVENT, (event) => {
				this.#dispatchOutput(event.payload);
			});
			const onClosed = await listen<TerminalClosed>(CLOSED_EVENT, (event) => {
				void this.#handleClosed(event.payload);
			});
			this.#unlisten.push(onOutput, onClosed);
		} catch {
			// Event-bus bind failed. Without it terminals can
			// only show their open error; better than a silent
			// hang.
		}
	}

	sessionFor(streamId: string): TerminalSession | undefined {
		return this.#sessions.get(streamId);
	}

	/** The shell-history line currently recorded for `streamId`,
	 * or `null` if nothing was ever observed. */
	commandFor(streamId: string): string | null {
		return this.#commands.get(streamId) ?? null;
	}

	/** Record `command` as the terminal's latest history line.
	 * Called by `TerminalTab` when it spots a prompt-render
	 * escape in the output stream (OSC 133/633 or a bare
	 * carriage return at a prompt). */
	recordCommand(streamId: string, command: string): void {
		const trimmed = command.trim();
		if (trimmed.length === 0) {
			return;
		}
		this.#commands.set(streamId, trimmed);
	}

	/** Snapshot of the restore list for `AppState.bottom_panel
	 * .terminals`: one entry per open terminal tab, in tab
	 * order, carrying its last-recorded history line. */
	serialisePersisted(): PersistedTerminal[] {
		const out: PersistedTerminal[] = [];
		for (const tab of bottomPanel.tabs) {
			if (tab.kind !== 'terminal') {
				continue;
			}
			const session = this.#sessions.get(tab.id);
			out.push({
				target: tab.target,
				folder: session?.folder ?? null,
				command: this.#commands.get(tab.id) ?? null,
			});
		}
		return out;
	}

	/** Stash the persisted restore list at launch. Pure state —
	 * nothing spawns until `restoreTerminals` runs, so the
	 * caller controls the timing (container status settled,
	 * event bus attached). */
	hydratePersisted(terminals: PersistedTerminal[]): void {
		this.#restoring = terminals;
	}

	/** Tabs hydrated from disk that haven't been replayed yet —
	 * the launcher surfaces them as one-click "re-open" entries
	 * if the automatic replay bailed (container never came up,
	 * user opened a log tab first). */
	get pendingRestore(): readonly PersistedTerminal[] {
		return this.#restoring;
	}

	/** Replay the hydrated terminal tabs: spawn a fresh shell
	 * per entry with its recorded command prefilled at the
	 * prompt (not executed — the user presses Enter). Container
	 * terminals wait for the workspace shell to reach `running`
	 * (the launch-time auto-resume can take minutes on an image
	 * pull); if it never does, the entries stay in
	 * `pendingRestore` for a manual re-open. Returns whether the
	 * replay ran — `false` means the panel already has tabs or
	 * nothing was hydrated, and the caller should fall back to
	 * its default single-terminal spawn. */
	async restoreTerminals(containerRefresh: Promise<void>, terminalRuntime: Promise<void>): Promise<boolean> {
		const entries = this.#restoring;
		this.#restoring = [];
		if (entries.length === 0) {
			return false;
		}
		if (bottomPanel.tabs.length > 0 || !bottomPanel.visible) {
			return false;
		}
		await containerRefresh;
		await terminalRuntime;
		if (bottomPanel.tabs.length > 0 || !bottomPanel.visible) {
			this.#restoring = entries;
			return true;
		}
		const wantsContainer = entries.some((e) => e.target.kind === 'container');
		if (wantsContainer && container.state !== 'running') {
			// Same posture as the old single-terminal auto-spawn:
			// defer to the auto-resume's `container:state` event
			// rather than erroring every container terminal out.
			const started = await container.onceRunning(60_000);
			if (!started) {
				this.#restoring = entries;
				return true;
			}
		}
		if (bottomPanel.tabs.length > 0 || !bottomPanel.visible) {
			this.#restoring = entries;
			return true;
		}
		for (const entry of entries) {
			if (entry.target.kind === 'container' && container.state !== 'running') {
				// Shouldn't happen after the gate above; skip
				// rather than seed an error tab.
				continue;
			}
			await this.open(entry.target, 80, 24, entry.folder, entry.command);
		}
		return true;
	}

	/**
	 * Open a new terminal session against `target`, register a
	 * `terminal` tab in the bottom panel, and return the stream
	 * id. The bottom panel becomes visible as a side effect —
	 * the user clicked + Terminal to see something.
	 *
	 * `folder` is the bound folder the terminal belongs to (the
	 * active project at open time). The backend records it so the
	 * coder's terminal-reading tools only ever see the terminals
	 * of the project a session is working in — see ADR 0048.
	 *
	 * `command` (restart / session replay) is prefilled at the
	 * fresh shell's prompt by the backend (not executed) and
	 * seeded into the tab's recorded history line.
	 */
	async open(
		target: TerminalTarget,
		cols: number,
		rows: number,
		folder: string | null,
		command: string | null = null,
	): Promise<string> {
		bottomPanel.show();

		const request: TerminalOpenRequest = { target, cols, rows, folder, command };
		let streamId: string;
		try {
			streamId = await ipc.terminal.open(request);
		} catch (err) {
			// Spawn failed (no shell, daemon down, container
			// gone). Mint a synthetic id and seed an errored
			// session so the body can render the message.
			streamId = `error-${cryptoRandomId()}`;
			this.#sessions.set(streamId, {
				streamId,
				target,
				folder,
				closedReason: null,
				openError: formatError(err),
			});
			if (command !== null) {
				this.#commands.set(streamId, command);
			}
			bottomPanel.addTab(this.#tabFor(streamId, target));
			this.#notify();
			return streamId;
		}

		this.#sessions.set(streamId, {
			streamId,
			target,
			folder,
			closedReason: null,
			openError: null,
		});
		if (command !== null) {
			this.#commands.set(streamId, command);
		}
		bottomPanel.addTab(this.#tabFor(streamId, target));
		this.#notify();
		return streamId;
	}

	async close(streamId: string): Promise<void> {
		const session = this.#sessions.get(streamId);
		if (!session) {
			bottomPanel.closeTab(streamId);
			this.#notify();
			return;
		}
		try {
			if (!session.closedReason && !session.openError) {
				await ipc.terminal.close(streamId);
			}
		} catch {
			// Backend close failed (window torn down). Local
			// cleanup proceeds regardless.
		}
		this.#sessions.delete(streamId);
		this.#writers.delete(streamId);
		this.#pending.delete(streamId);
		this.#commands.delete(streamId);
		if (this.activeSelection?.streamId === streamId) {
			this.activeSelection = null;
		}
		bottomPanel.closeTab(streamId);
		this.#notify();
	}

	/** Close every open terminal tab (e.g. the workspace was
	 * torn down). */
	async closeAll(): Promise<void> {
		const ids = bottomPanel.tabs.filter((t) => t.kind === 'terminal').map((t) => t.id);
		for (const id of ids) {
			await this.close(id);
		}
	}

	/** Re-spawn an exited terminal's shell in the same tab —
	 * the "restart" affordance on the container-loss banner.
	 * The old stream is closed (its registry entry frees), a
	 * fresh PTY opens against the same target with the recorded
	 * history line replayed, and the tab is re-pointed at the
	 * new stream without losing its strip position. No-op for
	 * live sessions. */
	async restart(streamId: string): Promise<void> {
		const session = this.#sessions.get(streamId);
		if (!session || session.closedReason === null) {
			return;
		}
		const tab = bottomPanel.tabs.find((t): t is TerminalTab => t.id === streamId && t.kind === 'terminal');
		if (!tab) {
			return;
		}
		const command = this.#commands.get(streamId) ?? null;
		if (session.target.kind === 'container' && container.state !== 'running') {
			// The banner's button gates on this too; a stale
			// click just gets ignored.
			return;
		}
		// Best-effort backend cleanup of the dead stream; the
		// supervisor's already gone so this is just the registry
		// forget. Local state is rebuilt from scratch below.
		try {
			await ipc.terminal.close(streamId);
		} catch {
			// Window mid-teardown — the new spawn's failure will
			// surface on its own tab.
		}
		const oldStreamId = streamId;
		let newStreamId: string;
		try {
			newStreamId = await ipc.terminal.open({
				target: session.target,
				cols: 80,
				rows: 24,
				folder: session.folder,
				command,
			});
		} catch (err) {
			this.#sessions.set(oldStreamId, { ...session, openError: formatError(err) });
			return;
		}
		this.#sessions.delete(oldStreamId);
		this.#writers.delete(oldStreamId);
		this.#pending.delete(oldStreamId);
		this.#sessions.set(newStreamId, {
			streamId: newStreamId,
			target: session.target,
			folder: session.folder,
			closedReason: null,
			openError: null,
		});
		if (command !== null) {
			this.#commands.delete(oldStreamId);
			this.#commands.set(newStreamId, command);
		}
		if (this.activeSelection?.streamId === oldStreamId) {
			this.activeSelection = null;
		}
		bottomPanel.replaceTabId(oldStreamId, newStreamId);
		this.#notify();
	}

	/** Register the xterm.js writer for a stream. Drains any
	 * output that arrived before the body was ready. */
	setWriter(streamId: string, writer: OutputWriter): void {
		this.#writers.set(streamId, writer);
		const queued = this.#pending.get(streamId);
		if (queued && queued.length > 0) {
			for (const chunk of queued) {
				writer(chunk);
			}
			this.#pending.delete(streamId);
		}
	}

	clearWriter(streamId: string): void {
		this.#writers.delete(streamId);
	}

	/** Update the cross-pane "last non-empty selection" snapshot.
	 * Empty strings clear the snapshot only when the *clearing*
	 * pane was the one whose selection we last cached — otherwise
	 * a user dragging across pane B would race with pane A's
	 * "selection cleared" event and we'd lose B's selection. */
	setSelection(streamId: string, text: string, label: string): void {
		if (text.length === 0) {
			if (this.activeSelection?.streamId === streamId) {
				this.activeSelection = null;
			}
			return;
		}
		this.activeSelection = { streamId, text, label };
	}

	async writeInput(streamId: string, bytes: Uint8Array): Promise<void> {
		const data = base64Encode(bytes);
		await ipc.terminal.write(streamId, data);
	}

	async resize(streamId: string, cols: number, rows: number): Promise<void> {
		await ipc.terminal.resize(streamId, cols, rows);
	}

	#dispatchOutput(payload: TerminalOutput): void {
		const bytes = base64Decode(payload.data);
		const writer = this.#writers.get(payload.stream_id);
		if (writer) {
			writer(bytes);
			return;
		}
		// No writer yet — the tab body hasn't mounted (or
		// it un-registered between paint frames). Queue
		// for the next [`setWriter`] call.
		const queue = this.#pending.get(payload.stream_id);
		if (queue) {
			queue.push(bytes);
			return;
		}
		this.#pending.set(payload.stream_id, [bytes]);
	}

	/** React to the backend's `terminal:closed`. Shell exits
	 * (the user's own Ctrl+D / `exit`, or a finished command —
	 * host always, container when the container itself is still
	 * up) close the tab outright: a shell that ends is done, and
	 * a dead tab strip was the old UX's main complaint. Container
	 * *losses* keep the tab so the banner can offer a respawn
	 * once the environment is back. */
	async #handleClosed(payload: TerminalClosed): Promise<void> {
		const session = this.#sessions.get(payload.stream_id);
		if (!session) {
			return;
		}
		switch (payload.reason) {
			case 'shell_exited':
			case 'container_shell_exited':
			case 'unknown':
				// `unknown` auto-closes too: portable-pty
				// couldn't translate the exit, but the process
				// is just as gone. Keeping a tab nobody can
				// reuse was worse than closing it.
				await this.close(payload.stream_id);
				return;
			case 'container_stopped':
			case 'container_not_running':
				this.#sessions.set(payload.stream_id, {
					...session,
					closedReason: payload.reason,
				});
				return;
		}
	}

	#tabFor(streamId: string, target: TerminalTarget): TerminalTab {
		return {
			id: streamId,
			title: terminalCwdBasename(target),
			kind: 'terminal',
			target,
		};
	}
}

/** Display name for a terminal tab — the cwd's basename, so
 * the tab strip stays scannable when several terminals are
 * open in different folders. Used as the static `tab.title`
 * (cwd doesn't change for the lifetime of a session). */
export function terminalCwdBasename(target: TerminalTarget): string {
	const cwd = target.kind === 'host' ? (target.cwd ?? '~') : target.cwd;
	if (cwd === '/' || cwd === '~') {
		return cwd;
	}
	const trimmed = cwd.replace(/\/+$/, '');
	if (trimmed.length === 0) {
		return cwd;
	}
	const slash = trimmed.lastIndexOf('/');
	if (slash < 0) {
		return trimmed;
	}
	const tail = trimmed.slice(slash + 1);
	return tail.length > 0 ? tail : cwd;
}

/** Marker suffix the tab strip shows for a terminal whose
 * environment died — empty string while live (and shell-exit
 * closes never show one: the tab closes itself). Reads the
 * store's reactive session map, so callers in a Svelte template
 * (e.g. `{@const}`) get a re-render on close. */
export function terminalExitSuffix(streamId: string): string {
	const session = terminal.sessionFor(streamId);
	if (!session) {
		return '';
	}
	if (session.openError) {
		return ' [failed]';
	}
	if (session.closedReason === null) {
		return '';
	}
	return ' [environment lost]';
}

function base64Encode(bytes: Uint8Array): string {
	let binary = '';
	for (const b of bytes) {
		binary += String.fromCharCode(b);
	}
	return btoa(binary);
}

function base64Decode(data: string): Uint8Array {
	const binary = atob(data);
	const out = new Uint8Array(binary.length);
	for (let i = 0; i < binary.length; i++) {
		out[i] = binary.charCodeAt(i);
	}
	return out;
}

function cryptoRandomId(): string {
	const bytes = new Uint8Array(8);
	crypto.getRandomValues(bytes);
	return Array.from(bytes, (b) => b.toString(16).padStart(2, '0')).join('');
}

export const terminal = new TerminalStore();
