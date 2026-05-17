//! Reactive store for PTY-backed terminal sessions.
//!
//! One [`TerminalSession`] per open terminal tab. The Tauri side
//! allocates the PTY and emits `terminal:output` chunks +
//! `terminal:closed` once on exit; we forward output bytes to
//! the matching xterm.js instance and mark the session closed
//! when the child finishes.
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
import { ipc } from './ipc';
import {
	formatError,
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
	/** Cleared on `closed` event; the body switches to a
	 * read-only "[exited (N)]" footer when this flips true. */
	closed: boolean;
	/** Exit code from the supervisor's `wait()`, or `null` for
	 * supervisor-aborted streams. */
	closeCode: number | null;
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
	#unlisten: UnlistenFn[] = [];
	#runtimeWired = false;

	/** Most recent non-empty selection across all open terminal
	 * panes. `null` when every pane has its selection cleared.
	 * Reactive: the editor's "Add to Coder" hint pill in
	 * `EditorPane.svelte` shouldn't read this (it's for editor
	 * selections only); App.svelte's Ctrl+L handler reads it as
	 * a fallback when the editor has nothing selected. */
	activeSelection = $state<TerminalSelectionSnapshot | null>(null);

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
				this.#markClosed(event.payload);
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

	/**
	 * Open a new terminal session against `target`, register a
	 * `terminal` tab in the bottom panel, and return the stream
	 * id. The bottom panel becomes visible as a side effect —
	 * the user clicked + Terminal to see something.
	 */
	async open(target: TerminalTarget, cols: number, rows: number): Promise<string> {
		bottomPanel.show();

		const request: TerminalOpenRequest = { target, cols, rows };
		let streamId: string;
		try {
			streamId = await ipc.terminal.open(request);
		} catch (err) {
			// Spawn failed (no shell, daemon down, container
			// gone). Mint a synthetic id and seed a closed
			// session so the body can render the error.
			streamId = `error-${cryptoRandomId()}`;
			this.#sessions.set(streamId, {
				streamId,
				target,
				closed: true,
				closeCode: null,
				openError: formatError(err),
			});
			bottomPanel.addTab(this.#tabFor(streamId, target));
			return streamId;
		}

		this.#sessions.set(streamId, {
			streamId,
			target,
			closed: false,
			closeCode: null,
			openError: null,
		});
		bottomPanel.addTab(this.#tabFor(streamId, target));
		return streamId;
	}

	async close(streamId: string): Promise<void> {
		const session = this.#sessions.get(streamId);
		if (!session) {
			bottomPanel.closeTab(streamId);
			return;
		}
		try {
			if (!session.closed && !session.openError) {
				await ipc.terminal.close(streamId);
			}
		} catch {
			// Backend close failed (window torn down). Local
			// cleanup proceeds regardless.
		}
		this.#sessions.delete(streamId);
		this.#writers.delete(streamId);
		this.#pending.delete(streamId);
		if (this.activeSelection?.streamId === streamId) {
			this.activeSelection = null;
		}
		bottomPanel.closeTab(streamId);
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

	#markClosed(payload: TerminalClosed): void {
		const session = this.#sessions.get(payload.stream_id);
		if (!session) {
			return;
		}
		this.#sessions.set(payload.stream_id, {
			...session,
			closed: true,
			closeCode: payload.code,
		});
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

/** Suffix appended to the tab title once the session is no
 * longer running — empty string while live. Reads the store's
 * reactive session map, so callers in a Svelte template (e.g.
 * `{@const}`) get a re-render on close. */
export function terminalExitSuffix(streamId: string): string {
	const session = terminal.sessionFor(streamId);
	if (!session) {
		return '';
	}
	if (session.openError) {
		return ' [failed]';
	}
	if (!session.closed) {
		return '';
	}
	if (session.closeCode === null) {
		return ' [exited]';
	}
	return ` [exited ${session.closeCode}]`;
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
