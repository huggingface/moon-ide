//! Reactive store for PTY-backed terminal sessions.
//!
//! One [`TerminalSession`] per open terminal tab. The Tauri side
//! allocates the PTY and emits `terminal:output` chunks +
//! `terminal:closed` once on exit; we forward output bytes to
//! the matching xterm.js instance. Whatever ended the shell — the
//! user's Ctrl+D, the container stopping — the tab closes with it
//! (ADR 0094).
//!
//! Persistence
//! -----------
//!
//! Terminal *tabs* persist across IDE launches (unlike log tabs —
//! see `bottomPanel.svelte.ts`): not the PTY, which dies with the
//! IDE, but the recipe — target, owning folder, and the
//! shell-history line the terminal last ran. `serialisePersisted`
//! snapshots the list into the per-workspace
//! `WorkspaceSession.terminals` (session.json — per-workspace, so
//! one workspace's terminals never leak into another's);
//! `hydratePersisted` + `restoreTerminals` replay it on launch by
//! spawning fresh shells and prefilling the recorded command at
//! each prompt. See ADR 0050.
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
	type AgentTerminal,
	type ContainerStateChange,
	type PersistedTerminal,
	type TerminalClosed,
	type TerminalAgentOpened,
	type TerminalOpenRequest,
	type TerminalOutput,
	type TerminalRemoved,
	type TerminalRespawned,
	type TerminalTarget,
} from './protocol';

const OUTPUT_EVENT = 'terminal:output';
const CLOSED_EVENT = 'terminal:closed';
const AGENT_OPENED_EVENT = 'terminal:agent_opened';
const RESPAWNED_EVENT = 'terminal:respawned';
const REMOVED_EVENT = 'terminal:removed';
const CONTAINER_STATE_EVENT = 'container:state';

/** Per-tab session state surfaced reactively to the body. */
export type TerminalSession = {
	streamId: string;
	target: TerminalTarget;
	/** Bound folder (host path) the terminal was opened for, or
	 * `null` for a folder-less `$HOME` shell. Kept for the
	 * persistence snapshot. */
	folder: string | null;
	/** Error returned by `terminal_open` itself. The tab still
	 * mounts so the message is visible. */
	openError: string | null;
	/** Set for a terminal an agent opened (ADR 0090): its label
	 * and launch command. Survives restart and relaunch. */
	agent: AgentTerminal | null;
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
			const onContainerState = await listen<ContainerStateChange>(CONTAINER_STATE_EVENT, (event) => {
				void this.#reconcileContainerState(event.payload.status.state);
			});
			const onAgentOpened = await listen<TerminalAgentOpened>(AGENT_OPENED_EVENT, (event) => {
				this.#adoptAgentTerminal(event.payload);
			});
			const onRespawned = await listen<TerminalRespawned>(RESPAWNED_EVENT, (event) => {
				this.#handleRespawned(event.payload.stream_id);
			});
			const onRemoved = await listen<TerminalRemoved>(REMOVED_EVENT, (event) => {
				this.#forgetLocal(event.payload.stream_id);
			});
			this.#unlisten.push(onOutput, onClosed, onContainerState, onAgentOpened, onRespawned, onRemoved);
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

	/** Snapshot of the restore list for
	 * `WorkspaceSession.terminals`: one entry per open terminal
	 * tab, in tab order, carrying its last-recorded history
	 * line. */
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
				agent: session?.agent ?? null,
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
			await this.open(entry.target, 80, 24, entry.folder, entry.command, entry.agent ?? null);
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
	 *
	 * `agent` restores an agent-opened terminal (ADR 0090) — the
	 * command is still only prefilled.
	 */
	async open(
		target: TerminalTarget,
		cols: number,
		rows: number,
		folder: string | null,
		command: string | null = null,
		agent: AgentTerminal | null = null,
	): Promise<string> {
		bottomPanel.show();

		const request: TerminalOpenRequest = { target, cols, rows, folder, command, agent };
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
				openError: formatError(err),
				agent,
			});
			if (command !== null) {
				this.#commands.set(streamId, command);
			}
			bottomPanel.addTab(this.#tabFor(streamId, target, agent));
			this.#notify();
			return streamId;
		}

		this.#sessions.set(streamId, {
			streamId,
			target,
			folder,
			openError: null,
			agent,
		});
		if (command !== null) {
			this.#commands.set(streamId, command);
		}
		bottomPanel.addTab(this.#tabFor(streamId, target, agent));
		this.#notify();
		return streamId;
	}

	/** Take in a terminal an agent opened (`terminal:agent_opened`,
	 * ADR 0090). The PTY already runs; this only makes the tab.
	 * The panel is revealed, but the new tab only comes to the
	 * front when the user isn't working in the panel — focus
	 * inside it means they're typing into another terminal, and
	 * switching tabs under them would eat their keystrokes. */
	#adoptAgentTerminal(payload: TerminalAgentOpened): void {
		if (this.#sessions.has(payload.stream_id)) {
			return;
		}
		const focused = document.activeElement;
		const userInPanel =
			bottomPanel.visible && focused !== null && focused.closest('[data-region="bottom-panel"]') !== null;
		bottomPanel.show();
		this.#sessions.set(payload.stream_id, {
			streamId: payload.stream_id,
			target: payload.target,
			folder: payload.folder,
			openError: null,
			agent: payload.agent,
		});
		this.#commands.set(payload.stream_id, payload.agent.command);
		bottomPanel.addTab(this.#tabFor(payload.stream_id, payload.target, payload.agent), !userInPanel);
		this.#notify();
	}

	/** The backend swapped the shell under an existing id (agent
	 * restart): clear a stale open error. */
	#handleRespawned(streamId: string): void {
		const session = this.#sessions.get(streamId);
		if (!session) {
			return;
		}
		this.#sessions.set(streamId, { ...session, openError: null });
	}

	async close(streamId: string): Promise<void> {
		const session = this.#sessions.get(streamId);
		if (!session) {
			bottomPanel.closeTab(streamId);
			this.#notify();
			return;
		}
		try {
			if (!session.openError) {
				await ipc.terminal.close(streamId);
			}
		} catch {
			// Backend close failed (window torn down). Local
			// cleanup proceeds regardless.
		}
		this.#forgetLocal(streamId);
	}

	/** Drop every trace of `streamId` on this side, for a terminal
	 * whose backend half is already gone. */
	#forgetLocal(streamId: string): void {
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

	/** The backend's `terminal:closed`: the shell is gone, so the
	 * tab goes too — whether the user exited it or the container
	 * stopped under it (ADR 0094). */
	async #handleClosed(payload: TerminalClosed): Promise<void> {
		if (!this.#sessions.has(payload.stream_id)) {
			return;
		}
		await this.close(payload.stream_id);
	}

	/** Close every container terminal when the workspace container
	 * reports non-running: their `docker exec`s are dying with it,
	 * and closing now beats waiting on each one's close event. */
	async #reconcileContainerState(state: ContainerStateChange['status']['state']): Promise<void> {
		if (state === 'running') {
			return;
		}
		const ids = [...this.#sessions.values()]
			.filter((session) => session.target.kind === 'container')
			.map((session) => session.streamId);
		for (const id of ids) {
			await this.close(id);
		}
	}

	#tabFor(streamId: string, target: TerminalTarget, agent: AgentTerminal | null): TerminalTab {
		return {
			id: streamId,
			title: agent?.title ?? terminalCwdBasename(target),
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

/** Marker suffix the tab strip shows for a terminal whose shell
 * failed to open — empty string otherwise. Reads the store's
 * reactive session map, so callers in a Svelte template (e.g.
 * `{@const}`) re-render when it changes. */
export function terminalExitSuffix(streamId: string): string {
	return terminal.sessionFor(streamId)?.openError ? ' [failed]' : '';
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
