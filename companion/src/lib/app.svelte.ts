// App state for the companion PWA. Svelte 5 runes, single shared
// store (same convention as the desktop app's `state.svelte.ts`).
//
// The companion is read-mostly for now (13.4): it pairs, lists
// workspaces, and shows a workspace's coder status + session list.
// Sending prompts / committing land when the backend exposes those
// relay methods (see specs/roadmaps/phase-13-mobile-companion.md).

import { BridgeSocket, clearConnection, loadConnection, type Connection } from './transport';

// Wire shapes mirror the bridge's read-only method results, which in
// turn mirror moon-coder / moon-core types. Kept minimal — only the
// fields the UI renders.
export type WorkspaceSnapshot = {
	id: string;
	folders: Array<{ path: string; name: string }>;
	active_folder: string | null;
};

export type WorkspaceListing = {
	id: string;
	name: string;
	last_active_at: number | null;
	live: boolean;
};

export type CoderStatus = {
	signed_in: boolean;
	running_turn: boolean;
};

export type SessionSummary = {
	id: string;
	title: string;
	updated_at_ms: number;
	/** Top-level session mode (ADR 0030); absent for the default
	 * `agent` mode, `"coordinator"` for an orchestrator session. */
	mode?: string | null;
};

/** A rendered transcript row. We collapse the coder's fine-grained
 * event grammar into three visible kinds for the phone. */
export type TranscriptRow =
	| { kind: 'user'; id: string; text: string }
	| { kind: 'assistant'; id: string; text: string }
	| { kind: 'tool'; id: string; name: string; status: 'running' | 'done' | 'error' };

// The coder event is an open set on the wire (the desktop emits many
// variants we don't render). We read it as a loose record and pull
// fields defensively per type, rather than a closed union that would
// choke on unknown variants.
type RawEvent = { type?: string; [key: string]: unknown };
type CoderEventEnvelope = { folder?: string; session_id?: string; event?: RawEvent };

function str(ev: RawEvent, key: string): string {
	const v = ev[key];
	return typeof v === 'string' ? v : '';
}

type Phase = 'connecting' | 'pairing' | 'ready' | 'error';

class CompanionState {
	phase = $state<Phase>('connecting');
	error = $state<string | null>(null);

	connection = $state<Connection | null>(null);
	#socket: BridgeSocket | null = null;

	/** Host workspaces (the switcher). */
	workspaces = $state<WorkspaceListing[]>([]);
	loadingWorkspaces = $state(false);

	/** The workspace the user picked, or null while choosing. */
	activeWorkspace = $state<string | null>(null);

	coderStatus = $state<CoderStatus | null>(null);
	sessions = $state<SessionSummary[]>([]);
	loadingSessions = $state(false);

	/** The session the user has opened on the phone, or null at the
	 * session list. */
	activeSession = $state<string | null>(null);
	/** Rendered transcript rows for the active session. */
	rows = $state<TranscriptRow[]>([]);
	/** True while a turn is streaming (composer shows abort). */
	busy = $state(false);
	subscribed = false;

	/** Boot: if we already have a paired connection, reconnect; else pair. */
	async boot(): Promise<void> {
		const conn = loadConnection();
		if (!conn) {
			this.phase = 'pairing';
			return;
		}
		this.connection = conn;
		try {
			this.#socket = new BridgeSocket(conn.url);
			await this.#socket.open();
			this.phase = 'ready';
		} catch (e) {
			this.error = e instanceof Error ? e.message : String(e);
			this.phase = 'error';
		}
	}

	/** Pair using the QR/typed payload. `url` is `wss://host:port`. */
	async pair(url: string, code: string, label: string): Promise<void> {
		this.error = null;
		try {
			const socket = new BridgeSocket(url);
			await socket.open();
			const conn = await socket.pair(code, label);
			this.#socket = socket;
			this.connection = conn;
			this.phase = 'ready';
		} catch (e) {
			this.error = e instanceof Error ? e.message : String(e);
		}
	}

	/** Forget this device's pairing and return to the pair screen. */
	unpair(): void {
		clearConnection();
		this.#socket?.close();
		this.#socket = null;
		this.connection = null;
		this.activeWorkspace = null;
		this.coderStatus = null;
		this.sessions = [];
		this.subscribed = false;
		this.closeSession();
		this.phase = 'pairing';
	}

	async #call<T>(workspace: string, method: string, params: unknown = {}): Promise<T> {
		if (!this.#socket || !this.connection) {
			throw new Error('not connected');
		}
		return this.#socket.call<T>(this.connection.token, workspace, method, params);
	}

	/** Load the host's workspace list for the switcher. */
	async loadWorkspaces(): Promise<void> {
		if (!this.#socket || !this.connection) {
			return;
		}
		this.loadingWorkspaces = true;
		this.error = null;
		try {
			this.workspaces = await this.#socket.workspaces<WorkspaceListing[]>(this.connection.token);
		} catch (e) {
			this.error = e instanceof Error ? e.message : String(e);
		} finally {
			this.loadingWorkspaces = false;
		}
	}

	/** Open a workspace: load its coder status + session list. */
	async openWorkspace(workspace: string): Promise<void> {
		this.activeWorkspace = workspace;
		this.coderStatus = null;
		this.sessions = [];
		this.loadingSessions = true;
		try {
			this.coderStatus = await this.#call<CoderStatus>(workspace, 'coder_status');
			this.sessions = await this.#call<SessionSummary[]>(workspace, 'coder_list_sessions');
		} catch (e) {
			this.error = e instanceof Error ? e.message : String(e);
		} finally {
			this.loadingSessions = false;
		}
	}

	/** Back out of the active workspace to the switcher. */
	closeWorkspace(): void {
		this.activeWorkspace = null;
		this.coderStatus = null;
		this.sessions = [];
		this.closeSession();
	}

	/** Open a session: load it on the backend, replay its transcript
	 * via the event stream, and subscribe to live events. */
	async openSession(id: string): Promise<void> {
		if (!this.activeWorkspace) {
			return;
		}
		this.activeSession = id;
		this.rows = [];
		this.busy = false;
		try {
			// Ensure we're receiving this workspace's event stream. The
			// open_session call replays the transcript as events.
			this.#ensureSubscribed(this.activeWorkspace);
			await this.#call(this.activeWorkspace, 'coder_open_session', { id });
		} catch (e) {
			this.error = e instanceof Error ? e.message : String(e);
		}
	}

	closeSession(): void {
		this.activeSession = null;
		this.rows = [];
		this.busy = false;
	}

	/** Send a prompt to the active session. */
	async sendPrompt(text: string): Promise<void> {
		if (!this.activeWorkspace || !text.trim()) {
			return;
		}
		try {
			this.busy = true;
			await this.#call(this.activeWorkspace, 'coder_send', { text });
		} catch (e) {
			this.busy = false;
			this.error = e instanceof Error ? e.message : String(e);
		}
	}

	/** Abort the running turn. */
	async abort(): Promise<void> {
		if (!this.activeWorkspace) {
			return;
		}
		try {
			await this.#call(this.activeWorkspace, 'coder_abort');
		} catch (e) {
			this.error = e instanceof Error ? e.message : String(e);
		}
	}

	#ensureSubscribed(workspace: string): void {
		if (this.subscribed || !this.#socket || !this.connection) {
			return;
		}
		this.#socket.onEvent((raw) => this.#onCoderEvent(raw));
		this.#socket.subscribe(this.connection.token, workspace);
		this.subscribed = true;
	}

	/** Reduce a coder event envelope onto the transcript rows. */
	#onCoderEvent(raw: unknown): void {
		// eslint-disable-next-line typescript-eslint/no-unsafe-type-assertion
		const envelope = (raw ?? {}) as CoderEventEnvelope;
		// Only render events for the session the phone has open. Other
		// sessions / folders stream too (the desktop may be busy) but
		// aren't shown here.
		if (this.activeSession && envelope.session_id && envelope.session_id !== this.activeSession) {
			return;
		}
		const ev = envelope.event;
		if (!ev) {
			return;
		}
		// A `replay` batch packs a whole session's historic events
		// into one envelope (desktop's `CoderEvent::Replay`, added
		// for IPC-batching on session open). Unpack and feed each
		// inner event back through this reducer so the phone sees
		// the same transcript it would from the per-event stream.
		if (ev.kind === 'replay') {
			const inner = ev.events;
			if (Array.isArray(inner)) {
				for (const e of inner) {
					this.#onCoderEvent({ ...envelope, event: e });
				}
			}
			// The batch ends with a `turn_complete` terminator that
			// clears `busy`; re-assert it when the reopened session is
			// still running in the background so the composer keeps
			// showing Stop instead of Send.
			if (ev.in_flight === true) {
				this.busy = true;
			}
			return;
		}
		if (typeof ev.type !== 'string') {
			return;
		}
		switch (ev.type) {
			case 'user_message':
				this.rows.push({ kind: 'user', id: str(ev, 'id'), text: str(ev, 'text') });
				break;
			case 'assistant_message_start':
				this.busy = true;
				this.rows.push({ kind: 'assistant', id: str(ev, 'id'), text: '' });
				break;
			case 'assistant_message_delta':
				this.#appendAssistant(str(ev, 'id'), str(ev, 'delta'));
				break;
			case 'assistant_message_end':
				this.#setAssistant(str(ev, 'id'), str(ev, 'text'));
				break;
			case 'tool_call':
				this.rows.push({ kind: 'tool', id: str(ev, 'id'), name: str(ev, 'name'), status: 'running' });
				break;
			case 'tool_result':
				this.#setToolStatus(str(ev, 'id'), ev['is_error'] === true ? 'error' : 'done');
				break;
			case 'turn_complete':
			case 'aborted':
				this.busy = false;
				break;
			case 'error':
				this.busy = false;
				this.error = str(ev, 'message') || 'coder error';
				break;
			default:
				break;
		}
	}

	#appendAssistant(id: string, delta: string): void {
		const row = this.rows.find((r) => r.kind === 'assistant' && r.id === id);
		if (row && row.kind === 'assistant') {
			row.text += delta;
		} else {
			this.rows.push({ kind: 'assistant', id, text: delta });
		}
	}

	#setAssistant(id: string, text: string): void {
		const row = this.rows.find((r) => r.kind === 'assistant' && r.id === id);
		if (row && row.kind === 'assistant') {
			row.text = text;
		}
	}

	#setToolStatus(id: string, status: 'running' | 'done' | 'error'): void {
		const row = this.rows.find((r) => r.kind === 'tool' && r.id === id);
		if (row && row.kind === 'tool') {
			row.status = status;
		}
	}
}

export const app = new CompanionState();
