// App state for the companion PWA. Svelte 5 runes, single shared
// store (same convention as the desktop app's `state.svelte.ts`).
//
// The companion can fully drive coder sessions: list, open, create,
// delete, send prompts, abort, answer ask_user prompts, and render
// the full event stream (thinking, tool calls with args, diffs, token
// usage, sub-agents, compaction, session metadata).

import { BridgeSocket, clearConnection, loadConnection, type Connection } from './transport';

// Wire shapes mirror the bridge's read-only method results, which in
// turn mirror moon-coder / moon-core types. Kept minimal — only the
// fields the UI renders.
export type WorkspaceFolder = {
	path: string;
	name: string;
	/** `{ kind: "user_picked" }` or `{ kind: "worktree", … }` —
	 * worktree folders are hidden from the phone's project switcher
	 * (they share their parent project's session list, ADR 0028). */
	origin?: { kind: string };
};

export type WorkspaceSnapshot = {
	id: string;
	folders: WorkspaceFolder[];
	active_folder: string | null;
};

export type WorkspaceListing = {
	id: string;
	name: string;
	last_active_at: number | null;
	live: boolean;
	/** Owning IDE's id (empty = local-carrier / this machine).
	 * Phase 14, ADR 0031 — the switcher groups by this. */
	ide?: string;
};

export type CoderStatus = {
	signed_in: boolean;
};

export type SessionSummary = {
	id: string;
	title: string;
	updated_at_ms: number;
	/** Top-level session mode (ADR 0030); absent for the default
	 * `agent` mode, `"coordinator"` for an orchestrator session. */
	mode?: string | null;
};

/** A rendered transcript row. The phone collapses the coder's
 * fine-grained event grammar into these visible kinds. */
export type TranscriptRow =
	| { kind: 'user'; id: string; text: string; queued: boolean }
	| { kind: 'assistant'; id: string; text: string; thinking: string }
	| {
			kind: 'tool';
			id: string;
			name: string;
			args: string;
			result: string;
			status: 'running' | 'done' | 'error';
	  }
	| {
			kind: 'ask_user';
			id: string;
			callId: string;
			questions: AskUserQuestion[];
			answered: boolean;
	  }
	| { kind: 'diff'; id: string; files: string[]; diff: string }
	| { kind: 'tokens'; id: string; total: number; contextWindow: number }
	| { kind: 'compaction'; id: string; summary: string; done: boolean }
	| { kind: 'subagent'; id: string; subagentId: string; folder: string; finished: boolean };

/** One question in an ask_user tool call. */
export type AskUserQuestion = {
	id: string;
	question: string;
	options: Array<{ id: string; label: string }>;
	multi: boolean;
};

/** A pending ask_user prompt awaiting the user's response. */
export type PendingPrompt = {
	callId: string;
	questions: AskUserQuestion[];
};

// The coder event is an open set on the wire. We read it as a loose
// record and pull fields defensively per type, rather than a closed
// union that would choke on unknown variants.
type RawEvent = { type?: string; kind?: string; [key: string]: unknown };
type CoderEventEnvelope = { folder?: string; session_id?: string; event?: RawEvent };

function str(ev: RawEvent, key: string): string {
	const v = ev[key];
	return typeof v === 'string' ? v : '';
}

function num(ev: RawEvent, key: string): number {
	const v = ev[key];
	return typeof v === 'number' ? v : 0;
}

function bool(ev: RawEvent, key: string): boolean {
	return ev[key] === true;
}

/** Parse ask_user tool args into the question shapes the UI needs. */
function parseAskUserArgs(args: unknown): AskUserQuestion[] {
	if (typeof args !== 'object' || args === null) {
		return [];
	}
	// eslint-disable-next-line typescript-eslint/no-unsafe-type-assertion
	const a = args as { questions?: unknown[] };
	if (!Array.isArray(a.questions)) {
		return [];
	}
	return a.questions
		.map((q): AskUserQuestion | null => {
			if (typeof q !== 'object' || q === null) {
				return null;
			}
			// eslint-disable-next-line typescript-eslint/no-unsafe-type-assertion
			const qo = q as {
				id?: string;
				question?: string;
				options?: Array<{ id?: string; label?: string }>;
				multi?: boolean;
			};
			return {
				id: qo.id ?? '',
				question: qo.question ?? '',
				options: (qo.options ?? []).map((o) => ({
					id: o.id ?? '',
					label: o.label ?? '',
				})),
				multi: qo.multi === true,
			};
		})
		.filter((q): q is AskUserQuestion => q !== null);
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
	/** Human-readable name of the active workspace (falls back to
	 * the slug when the listing had none). */
	activeWorkspaceName = $state('');
	/** The owning IDE's id for the active workspace (empty = local). */
	activeIde = $state('');

	/** Bound folders of the active workspace (the project switcher).
	 * Worktree folders are filtered out — they share their parent
	 * project's session list. */
	folders = $state<WorkspaceFolder[]>([]);
	/** The folder (project) whose sessions the phone is browsing. */
	activeFolder = $state<string | null>(null);

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
	/** True when an ask_user prompt is blocking the turn. */
	awaitingInput = $state(false);
	/** The pending ask_user prompt, if awaitingInput. */
	pendingPrompt = $state<PendingPrompt | null>(null);
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
		this.activeWorkspaceName = '';
		this.folders = [];
		this.activeFolder = null;
		this.coderStatus = null;
		this.sessions = [];
		this.subscribed = false;
		this.closeSession();
		this.phase = 'pairing';
	}

	/** Clear the visible error (the banner's dismiss button). */
	dismissError(): void {
		this.error = null;
	}

	/** Reconnect after the PWA was backgrounded (a backgrounded tab's
	 * WebSocket drops; the visibilitychange handler in `App.svelte`
	 * calls this on resume). Re-opens the socket, re-subscribes the
	 * event stream, and re-syncs the screen the user was on. */
	async ensureConnected(): Promise<void> {
		if (this.phase !== 'ready' || !this.connection || this.#reconnecting) {
			return;
		}
		if (this.#socket?.isOpen()) {
			return;
		}
		this.#reconnecting = true;
		try {
			const socket = new BridgeSocket(this.connection.url);
			await socket.open();
			this.#socket?.close();
			this.#socket = socket;
			this.subscribed = false;
			this.error = null;
			if (!this.activeWorkspace) {
				await this.loadWorkspaces();
				return;
			}
			this.#ensureSubscribed(this.activeWorkspace, this.activeIde);
			await this.#refreshSessions();
			if (this.activeSession) {
				// Re-open to replay whatever streamed while we were
				// backgrounded. Best-effort: a fresh session that never
				// persisted has no JSONL yet, and its rows are still in
				// memory anyway.
				try {
					this.rows = [];
					await this.#call(
						this.activeWorkspace,
						'coder_open_session',
						{ id: this.activeSession, folder: this.activeFolder },
						this.activeIde,
					);
				} catch {
					// Keep the in-memory transcript; the next send re-syncs.
				}
			}
		} catch (e) {
			this.error = e instanceof Error ? e.message : String(e);
		} finally {
			this.#reconnecting = false;
		}
	}
	#reconnecting = false;

	async #call<T>(workspace: string, method: string, params: unknown = {}, ide = ''): Promise<T> {
		if (!this.#socket || !this.connection) {
			throw new Error('not connected');
		}
		return this.#socket.call<T>(this.connection.token, workspace, method, params, ide);
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

	/** Open a workspace: load its folder list (the project switcher),
	 * coder status, and the active folder's session list. */
	async openWorkspace(workspace: string, ide = '', name = ''): Promise<void> {
		this.activeWorkspace = workspace;
		this.activeWorkspaceName = name || workspace;
		this.activeIde = ide;
		this.folders = [];
		this.activeFolder = null;
		this.coderStatus = null;
		this.sessions = [];
		this.error = null;
		this.loadingSessions = true;
		try {
			const snap = await this.#call<WorkspaceSnapshot>(workspace, 'workspace_snapshot', {}, ide);
			this.folders = snap.folders.filter((f) => f.origin?.kind !== 'worktree');
			// Default to the workspace's active folder when it's a
			// switchable project; a worktree active folder falls back
			// to the first project.
			const active = this.folders.find((f) => f.path === snap.active_folder);
			this.activeFolder = active?.path ?? this.folders[0]?.path ?? null;
			this.coderStatus = await this.#call<CoderStatus>(workspace, 'coder_status', {}, ide);
			this.sessions = await this.#loadSessions();
		} catch (e) {
			this.error = e instanceof Error ? e.message : String(e);
		} finally {
			this.loadingSessions = false;
		}
	}

	/** Switch the phone to another project (bound folder) inside the
	 * active workspace. Purely phone-side targeting — the desktop's
	 * active folder is untouched. */
	async openFolder(path: string): Promise<void> {
		if (!this.activeWorkspace || this.activeFolder === path) {
			return;
		}
		this.activeFolder = path;
		this.closeSession();
		this.sessions = [];
		this.error = null;
		this.loadingSessions = true;
		try {
			this.sessions = await this.#loadSessions();
		} catch (e) {
			this.error = e instanceof Error ? e.message : String(e);
		} finally {
			this.loadingSessions = false;
		}
	}

	async #loadSessions(): Promise<SessionSummary[]> {
		if (!this.activeWorkspace) {
			return [];
		}
		return this.#call<SessionSummary[]>(
			this.activeWorkspace,
			'coder_list_sessions',
			{ folder: this.activeFolder },
			this.activeIde,
		);
	}

	/** Back out of the active workspace to the switcher. */
	closeWorkspace(): void {
		this.activeWorkspace = null;
		this.activeWorkspaceName = '';
		this.activeIde = '';
		this.folders = [];
		this.activeFolder = null;
		this.coderStatus = null;
		this.sessions = [];
		this.error = null;
		this.closeSession();
	}

	/** Create a new coder session and show it. The blank session is
	 * only mounted in memory (nothing on disk until the first send),
	 * so we deliberately don't `coder_open_session` it — that loads
	 * the JSONL and would error with "no such file". */
	async newSession(): Promise<void> {
		if (!this.activeWorkspace) {
			return;
		}
		try {
			const summary = await this.#call<SessionSummary>(
				this.activeWorkspace,
				'coder_new_session',
				{ folder: this.activeFolder },
				this.activeIde,
			);
			this.sessions = [summary, ...this.sessions];
			this.#ensureSubscribed(this.activeWorkspace, this.activeIde);
			this.activeSession = summary.id;
			this.rows = [];
			this.busy = false;
			this.awaitingInput = false;
			this.pendingPrompt = null;
			this.error = null;
		} catch (e) {
			this.error = e instanceof Error ? e.message : String(e);
		}
	}

	/** Delete a session by id. */
	async deleteSession(id: string): Promise<void> {
		if (!this.activeWorkspace) {
			return;
		}
		try {
			await this.#call(this.activeWorkspace, 'coder_delete_session', { id, folder: this.activeFolder }, this.activeIde);
			this.sessions = this.sessions.filter((s) => s.id !== id);
			if (this.activeSession === id) {
				this.closeSession();
			}
		} catch (e) {
			this.error = e instanceof Error ? e.message : String(e);
		}
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
		this.awaitingInput = false;
		this.pendingPrompt = null;
		this.error = null;
		try {
			this.#ensureSubscribed(this.activeWorkspace, this.activeIde);
			await this.#call(this.activeWorkspace, 'coder_open_session', { id, folder: this.activeFolder }, this.activeIde);
		} catch (e) {
			this.error = e instanceof Error ? e.message : String(e);
		}
	}

	closeSession(): void {
		this.activeSession = null;
		this.rows = [];
		this.busy = false;
		this.awaitingInput = false;
		this.pendingPrompt = null;
	}

	/** Send a prompt to the session the phone has open. Targeted by
	 * `session_id` so it can't land in whatever session the desktop
	 * happens to have visible. */
	async sendPrompt(text: string): Promise<void> {
		if (!this.activeWorkspace || !this.activeSession || !text.trim()) {
			return;
		}
		try {
			this.busy = true;
			await this.#call(this.activeWorkspace, 'coder_send', { text, session_id: this.activeSession }, this.activeIde);
		} catch (e) {
			this.busy = false;
			this.error = e instanceof Error ? e.message : String(e);
		}
	}

	/** Abort the open session's running turn. */
	async abort(): Promise<void> {
		if (!this.activeWorkspace || !this.activeSession) {
			return;
		}
		try {
			await this.#call(this.activeWorkspace, 'coder_abort', { session_id: this.activeSession }, this.activeIde);
		} catch (e) {
			this.error = e instanceof Error ? e.message : String(e);
		}
	}

	/** Respond to an ask_user prompt. */
	async respondToPrompt(
		callId: string,
		answers: Array<{ question_id: string; selected: string[]; free_text: string }>,
	): Promise<void> {
		if (!this.activeWorkspace) {
			return;
		}
		try {
			await this.#call(
				this.activeWorkspace,
				'coder_respond_to_prompt',
				{ call_id: callId, response: { answers } },
				this.activeIde,
			);
			this.awaitingInput = false;
			this.pendingPrompt = null;
			// Mark the ask_user row as answered.
			const row = this.rows.find((r) => r.kind === 'ask_user' && r.callId === callId);
			if (row && row.kind === 'ask_user') {
				row.answered = true;
			}
		} catch (e) {
			this.error = e instanceof Error ? e.message : String(e);
		}
	}

	#ensureSubscribed(workspace: string, ide = ''): void {
		if (this.subscribed || !this.#socket || !this.connection) {
			return;
		}
		this.#socket.onEvent((raw) => this.#onCoderEvent(raw));
		this.#socket.subscribe(this.connection.token, workspace, ide);
		this.subscribed = true;
	}

	/** Reduce a coder event envelope onto the transcript rows. */
	#onCoderEvent(raw: unknown): void {
		// eslint-disable-next-line typescript-eslint/no-unsafe-type-assertion
		const envelope = (raw ?? {}) as CoderEventEnvelope;
		// Only render events for the session the phone has open.
		if (this.activeSession && envelope.session_id && envelope.session_id !== this.activeSession) {
			return;
		}
		const ev = envelope.event;
		if (!ev) {
			return;
		}
		// A `replay` batch packs a whole session's historic events
		// into one envelope. Unpack and feed each inner event back
		// through this reducer.
		if (ev.kind === 'replay') {
			const inner = ev.events;
			if (Array.isArray(inner)) {
				for (const e of inner) {
					this.#onCoderEvent({ ...envelope, event: e });
				}
			}
			if (bool(ev, 'in_flight')) {
				this.busy = true;
			}
			return;
		}
		if (typeof ev.type !== 'string') {
			return;
		}
		switch (ev.type) {
			case 'user_message':
				this.rows.push({
					kind: 'user',
					id: str(ev, 'id'),
					text: str(ev, 'text'),
					queued: bool(ev, 'queued'),
				});
				break;
			case 'steer_drained':
				this.rows = this.rows.filter((r) => !(r.kind === 'user' && r.id === str(ev, 'id')));
				break;
			case 'assistant_message_start':
				this.busy = true;
				this.rows.push({ kind: 'assistant', id: str(ev, 'id'), text: '', thinking: '' });
				break;
			case 'assistant_message_delta':
				this.#appendAssistant(str(ev, 'id'), str(ev, 'delta'), '');
				break;
			case 'assistant_thinking_delta':
				this.#appendAssistant('', '', str(ev, 'delta'));
				break;
			case 'assistant_message_end':
				this.#setAssistant(str(ev, 'id'), str(ev, 'text'), str(ev, 'thinking'));
				break;
			case 'tool_call': {
				const name = str(ev, 'name');
				const args = ev['args'];
				const argsStr = typeof args === 'object' ? JSON.stringify(args) : str(ev, 'args');
				// ask_user gets its own row kind so the UI can render
				// the interactive prompt.
				if (name === 'ask_user') {
					const questions = parseAskUserArgs(args);
					const callId = str(ev, 'id');
					this.rows.push({
						kind: 'ask_user',
						id: callId,
						callId,
						questions,
						answered: false,
					});
					this.awaitingInput = true;
					this.pendingPrompt = { callId, questions };
				} else {
					this.rows.push({
						kind: 'tool',
						id: str(ev, 'id'),
						name,
						args: argsStr,
						result: '',
						status: 'running',
					});
				}
				break;
			}
			case 'tool_result': {
				const id = str(ev, 'id');
				const isError = bool(ev, 'is_error');
				// If this is the result of an ask_user, clear the
				// awaitingInput flag.
				const askRow = this.rows.find((r) => r.kind === 'ask_user' && r.callId === id);
				if (askRow && askRow.kind === 'ask_user') {
					this.awaitingInput = false;
					this.pendingPrompt = null;
				} else {
					const result = ev['result'];
					const resultStr = typeof result === 'string' ? result : JSON.stringify(result);
					this.#setToolResult(id, resultStr, isError ? 'error' : 'done');
				}
				break;
			}
			case 'turn_complete':
			case 'aborted':
				this.busy = false;
				this.awaitingInput = false;
				this.pendingPrompt = null;
				break;
			case 'error':
				this.busy = false;
				this.error = str(ev, 'message') || 'coder error';
				break;
			case 'session_loaded':
				// Update the session title in the list if it changed.
				this.#updateSessionTitle(str(ev, 'id'), str(ev, 'title'));
				break;
			case 'session_title_updated':
				this.#updateSessionTitle(str(ev, 'id'), str(ev, 'title'));
				break;
			case 'session_list_changed':
				// Refresh the session list from the backend — but only
				// when the change happened in the folder the phone is
				// browsing (the envelope's folder is the coder root).
				if (this.activeWorkspace && (!envelope.folder || envelope.folder === this.activeFolder)) {
					void this.#refreshSessions();
				}
				break;
			case 'token_usage': {
				const total = num(ev, 'total_tokens');
				const ctx = num(ev, 'context_window');
				if (total > 0) {
					this.rows.push({
						kind: 'tokens',
						id: nextRowId('tok'),
						total,
						contextWindow: ctx,
					});
				}
				break;
			}
			case 'turn_diff': {
				const files = ev['files'];
				const diff = str(ev, 'diff');
				const fileList = Array.isArray(files) ? files.map(String) : [];
				if (fileList.length > 0 || diff) {
					this.rows.push({
						kind: 'diff',
						id: nextRowId('diff'),
						files: fileList,
						diff,
					});
				}
				break;
			}
			case 'compaction_started':
				this.rows.push({
					kind: 'compaction',
					id: nextRowId('comp'),
					summary: '',
					done: false,
				});
				break;
			case 'compaction_complete': {
				const summary = str(ev, 'summary');
				const row = this.rows.findLast((r) => r.kind === 'compaction' && !r.done);
				if (row && row.kind === 'compaction') {
					row.summary = summary;
					row.done = true;
				}
				break;
			}
			case 'subagent_spawned':
				this.rows.push({
					kind: 'subagent',
					id: `sub-${str(ev, 'subagent_id')}`,
					subagentId: str(ev, 'subagent_id'),
					folder: str(ev, 'target_folder'),
					finished: false,
				});
				break;
			case 'subagent_finished': {
				const sid = str(ev, 'subagent_id');
				const row = this.rows.findLast((r) => r.kind === 'subagent' && r.subagentId === sid);
				if (row && row.kind === 'subagent') {
					row.finished = true;
				}
				break;
			}
			default:
				break;
		}
	}

	async #refreshSessions(): Promise<void> {
		if (!this.activeWorkspace) {
			return;
		}
		try {
			this.sessions = await this.#loadSessions();
		} catch {
			// Silent — the list will refresh on next manual open.
		}
	}

	#updateSessionTitle(id: string, title: string): void {
		const session = this.sessions.find((s) => s.id === id);
		if (session) {
			session.title = title;
		}
	}

	#appendAssistant(id: string, delta: string, thinkingDelta: string): void {
		// If id is empty, it's a thinking delta — append to the last
		// assistant row's thinking field.
		if (!id) {
			const row = this.rows.findLast((r) => r.kind === 'assistant');
			if (row && row.kind === 'assistant') {
				row.thinking += thinkingDelta;
			}
			return;
		}
		const row = this.rows.find((r) => r.kind === 'assistant' && r.id === id);
		if (row && row.kind === 'assistant') {
			row.text += delta;
			row.thinking += thinkingDelta;
		} else {
			this.rows.push({ kind: 'assistant', id, text: delta, thinking: thinkingDelta });
		}
	}

	#setAssistant(id: string, text: string, thinking: string): void {
		const row = this.rows.find((r) => r.kind === 'assistant' && r.id === id);
		if (row && row.kind === 'assistant') {
			row.text = text;
			if (thinking) {
				row.thinking = thinking;
			}
		}
	}

	#setToolResult(id: string, result: string, status: 'done' | 'error'): void {
		const row = this.rows.find((r) => r.kind === 'tool' && r.id === id);
		if (row && row.kind === 'tool') {
			row.result = result;
			row.status = status;
		}
	}
}

/** Monotonic id for synthetic transcript rows (tokens, diff,
 *  compaction) whose backing events carry no id. Timestamps are
 *  not valid keys: a `replay` batch reduces synchronously, so two
 *  same-kind events land in the same millisecond and collide in
 *  the keyed `{#each}`. */
let syntheticRowSeq = 0;
function nextRowId(prefix: string): string {
	syntheticRowSeq += 1;
	return `${prefix}-${syntheticRowSeq}`;
}

export const app = new CompanionState();
