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

/** One user-added provider (mirror of `CoderProviderConfig`; only
 * the fields the phone renders, the rest round-trips untouched). */
export type ProviderEntry = {
	id: string;
	label: string;
	[key: string]: unknown;
};

/** Per-workspace provider lock (mirror of `CoderProviderLock`). */
export type ProviderLock = { kind: 'hf' } | { kind: 'user'; id: string };

/** SCM status for a bound folder (mirrors the bridge's
 * `workspace_scm_status` response — itself a composite of
 * `GitBranchInfo` + `git_status_entries`, folded the same way
 * `fs_git_change_summary` / the coordinator's `workspace_scm_status`
 * tool fold: untracked → added, conflicted → modified). */
type ScmStatus = {
	/** Optional on the wire: an older enrolled IDE, or a folder
	 * whose SCM probe failed, can return a status with no `branch`
	 * object. The card renders only when it's present (and `changes`
	 * / `files` still apply for the commit list). */
	branch?: {
		name: string | null;
		head_short_sha: string | null;
		has_upstream: boolean;
		ahead: number;
		behind: number;
		/** e.g. "origin/main"; null when there's no remote default. */
		default_branch_remote_ref?: string | null;
		/** Commits the default branch has that HEAD doesn't. */
		default_branch_behind?: number;
		/** Branch `git switch -` would return to; null when there's no
		 * recorded previous branch. Drives the "switch back" chip on the
		 * default branch. */
		previous_branch?: string | null;
	};
	/** Same wire-tolerance as `branch`: a partial payload from a
	 * mismatched IDE build must hide the changes card, not crash
	 * the view. */
	changes?: { added: number; modified: number; deleted: number; total: number };
	files?: { path: string; status: string }[];
};

/** Result of `workspace_scm_commit` (mirrors `GitCommitResult`). */
type ScmCommitResult = {
	short_sha: string;
	summary: string;
};

/** Mirror of `CoderModelSettings` — the read/write payload of
 * `coder_get_model_settings` / `coder_set_model_settings`. The index
 * signature keeps fields the phone doesn't know about round-tripping
 * unmodified on writes. */
export type ModelSettings = {
	active_provider?: string | null;
	providers: ProviderEntry[];
	provider_lock?: ProviderLock | null;
	/** Per-slug context-window caps in tokens (mirror of
	 * `CoderModelSettings::context_window_overrides`). The runner
	 * clamps `min(catalog, cap)`, so the usage ring and auto-
	 * compaction already respect it — this map is how it's edited. */
	context_window_overrides?: Record<string, number>;
	/** Effective standard-model slug (runner's fallback chain
	 * applied) — the model the usage ring / cap editor targets.
	 * Read-only on the wire. */
	resolved_standard_model?: string;
	[key: string]: unknown;
};

export type SessionSummary = {
	id: string;
	title: string;
	updated_at_ms: number;
	/** Absolute path of the git worktree this session drives
	 * (ADR 0028); absent for a main-tree session. The review view
	 * passes it as the `folder` target so an isolated agent's work
	 * is diffed in its own checkout. */
	worktree_root?: string | null;
	/** Branch of that worktree (`moon/agent-…` / `moon/<name>`). */
	worktree_branch?: string | null;
	/** Top-level session mode (ADR 0030); absent for the default
	 * `agent` mode, `"coordinator"` for an orchestrator session. */
	mode?: string | null;
};

/** A rendered transcript row. The phone collapses the coder's
 * fine-grained event grammar into these visible kinds. */
export type TranscriptRow =
	| { kind: 'user'; id: string; text: string; queued: boolean; fromCoordinator: boolean }
	| { kind: 'assistant'; id: string; text: string; thinking: string }
	| {
			kind: 'tool';
			id: string;
			name: string;
			args: string;
			result: string;
			/** Images the tool returned (a `read_file` on an image,
			 *  an MCP screenshot), extracted from the result
			 *  payload at the event boundary so the preview
			 *  text never carries the base64. */
			images: ToolImage[];
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
	| {
			kind: 'subagent';
			id: string;
			subagentId: string;
			folder: string;
			finished: boolean;
			/** ADR 0053 — a detached `task` runs in the background. */
			detached: boolean;
	  };

/** One question in an ask_user tool call. */
export type AskUserQuestion = {
	id: string;
	question: string;
	options: Array<{ id: string; label: string }>;
	multi: boolean;
};

/** One image a tool returned, in renderable form. Mirrors the
 * runner's `"images": [{ data_url, mime }]` convention. */
export type ToolImage = { dataUrl: string; mime: string };

/** Extract the `images` key of a tool-result payload. `[]` for
 * anything else — error envelopes, text-only results, old
 * traces. */
function toolImagesOf(result: unknown): ToolImage[] {
	const o = asRecord(result);
	if (o === null || !Array.isArray(o.images)) {
		return [];
	}
	const out: ToolImage[] = [];
	for (const item of o.images) {
		const img = asRecord(item);
		if (img !== null && typeof img.data_url === 'string' && img.data_url.length > 0) {
			out.push({ dataUrl: img.data_url, mime: typeof img.mime === 'string' ? img.mime : 'image' });
		}
	}
	return out;
}

/** The payload minus its `images` key, for text preview — the
 * raw JSON would otherwise dump megabytes of base64. */
function withoutToolImages(result: unknown): unknown {
	const o = asRecord(result);
	if (o === null || !('images' in o)) {
		return result;
	}
	const { images: _images, ...rest } = o;
	return rest;
}

/** Narrow an `unknown` payload to a string-keyed record without
 * a cast the linter flags — the `typeof` check is the guard. */
function asRecord(value: unknown): Record<string, unknown> | null {
	if (typeof value !== 'object' || value === null || Array.isArray(value)) {
		return null;
	}
	return { ...value };
}

/** A pending ask_user prompt awaiting the user's response. */
export type PendingPrompt = {
	callId: string;
	questions: AskUserQuestion[];
};

/** Reply shape of the bridge's observe-open (`coder_open_session`):
 * the transcript replay rides in the response rather than the event
 * stream. `events` are raw `CoderEvent`s fed through the same
 * reducer live events use. */
type ObservedSession = {
	summary: SessionSummary;
	events?: RawEvent[];
	in_flight?: boolean;
	/** True when `events` is the windowed tail of a longer
	 * transcript; the first event is then a `history_window_start`
	 * carrying the ordinal to resume pagination from. */
	has_more?: boolean;
};

/** Reply shape of `coder_session_history_older` (upward
 * pagination). `events` are plain replay events to prepend. */
type HistoryWindow = {
	events?: RawEvent[];
	has_more: boolean;
	before_event_ordinal: number;
	total_events: number;
};

// The coder event is an open set on the wire (`CoderEvent`, tagged
// `kind`). We read it as a loose record and pull fields defensively
// per kind, rather than a closed union that would choke on unknown
// variants.
type RawEvent = { kind?: string; [key: string]: unknown };
/** `ide` / `workspace` are the carrier tags the bridge stamps onto
 * relayed envelopes (empty `ide` = local carrier). Absent on frames
 * from an older bridge and on locally-synthesised replay envelopes —
 * both mean "don't filter". */
type CoderEventEnvelope = { folder?: string; session_id?: string; event?: RawEvent; ide?: string; workspace?: string };

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
	/** Model/provider settings for the open workspace, or null while
	 * loading / when the workspace's IDE predates the methods. */
	modelSettings = $state<ModelSettings | null>(null);
	/** True while a provider switch / lock toggle is in flight. */
	savingProvider = $state(false);
	/** SCM status for the active folder, or null while loading. */
	scmStatus = $state<ScmStatus | null>(null);
	/** True while fetching SCM status. */
	loadingScm = $state(false);
	/** True while a commit is in flight. */
	committing = $state(false);
	/** True while a push/pull/fetch is in flight. */
	scmBusy = $state(false);
	sessions = $state<SessionSummary[]>([]);
	loadingSessions = $state(false);

	/** The session the user has opened on the phone, or null at the
	 * session list. */
	activeSession = $state<string | null>(null);
	/** Rendered transcript rows for the active session. */
	rows = $state<TranscriptRow[]>([]);
	/** True when the open session has older history on disk beyond
	 * what `rows` holds (the open was windowed). Drives the "load
	 * older" affordance. */
	hasMoreHistory = $state(false);
	/** Full-sequence ordinal where the currently-loaded window
	 * begins; the exclusive upper bound for the next older page. */
	#oldestEventOrdinal = 0;
	/** True while an older page is being fetched. */
	loadingOlder = $state(false);
	/** Rows the most recent older-page fetch prepended — the view
	 * expands its render window by this so the new rows show. */
	lastOlderPageRows = $state(0);
	/** When set, `#onCoderEvent` and its helpers push into this
	 * array instead of `this.rows` — used to reduce an older-page
	 * fetch into a throwaway buffer that gets prepended in one
	 * assignment. */
	#rowsOverride: TranscriptRow[] | null = null;
	/** True while the open session's turn is streaming (composer
	 * shows abort). */
	busy = $state(false);
	/** Latest token usage for the open session, or null. Derived
	 * from the transcript's tokens row (updated in place by the
	 * `token_usage` event handler). The SessionView renders this in
	 * a sticky bar so it stays visible during streaming. */
	get tokenUsage(): { total: number; contextWindow: number; pct: number } | null {
		const row = this.rows.findLast((r) => r.kind === 'tokens');
		if (!row || row.kind !== 'tokens' || row.total === 0) {
			return null;
		}
		return {
			total: row.total,
			contextWindow: row.contextWindow,
			pct: row.contextWindow > 0 ? Math.round((row.total / row.contextWindow) * 100) : 0,
		};
	}

	/** Sessions in the current folder that have a running turn,
	 * tracked from the event stream (any `user_message` without a
	 * matching `turn_complete` / `aborted` / `error`). Drives the
	 * running pip in the session list — updated for *all* sessions,
	 * not just the open one, so a background session's pip stays
	 * lit while the user browses the list. */
	busySessions = $state<Set<string>>(new Set());
	/** Folders (project paths) where a live turn finished while the
	 * phone was looking elsewhere — the project chip's "finished"
	 * dot. Cleared when the user opens the folder. */
	folderAttention = $state<Set<string>>(new Set());
	/** Which folder each busy session runs in (from the event
	 * envelope), for the project chips' running pips. */
	#sessionFolder = new Map<string, string>();

	/** Folders with at least one running turn. Recomputed when
	 * `busySessions` changes (the map entry is written in the same
	 * tick as the set replacement). */
	get busyFolders(): Set<string> {
		const out = new Set<string>();
		for (const sid of this.busySessions) {
			const folder = this.#sessionFolder.get(sid);
			if (folder) {
				out.add(folder);
			}
		}
		return out;
	}
	/** True when an ask_user prompt is blocking the turn. */
	awaitingInput = $state(false);
	/** The pending ask_user prompt, if awaitingInput. */
	pendingPrompt = $state<PendingPrompt | null>(null);
	/** `(ide, workspace)` pairs the current socket already has an
	 * event subscription for. Per-workspace, not per-socket — a
	 * global boolean silently left every workspace after the first
	 * one without live events. Cleared on reconnect / unpair. */
	#subscriptions = new Set<string>();

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
		this.modelSettings = null;
		this.scmStatus = null;
		this.sessions = [];
		this.busySessions = new Set();
		this.folderAttention = new Set();
		this.#sessionFolder.clear();
		this.#subscriptions.clear();
		this.closeSession();
		this.phase = 'pairing';
	}

	/** Clear the visible error (the banner's dismiss button). */
	dismissError(): void {
		this.error = null;
	}

	/** Launch a stopped workspace on its host. For a local-carrier
	 * workspace (empty `ide`), the bridge spawns the desktop binary
	 * directly. For a remote-carrier workspace, the bridge forwards
	 * to the owning enrolled IDE, which runs the same "focus or
	 * spawn" path as the desktop's `window_open`. */
	async launchWorkspace(workspace: string, ide = ''): Promise<void> {
		if (!this.#socket || !this.connection) {
			return;
		}
		this.error = null;
		try {
			await this.#call(workspace, 'workspace_launch', { workspace_id: workspace }, ide);
			// Poll the workspace list so the phone sees it go live.
			// The new process takes a moment to bind its socket; a
			// single re-fetch after a short delay catches it, and
			// the user can pull-to-refresh if they're early.
			setTimeout(() => void this.loadWorkspaces(), 1500);
		} catch (e) {
			this.error = e instanceof Error ? e.message : String(e);
		}
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
			this.#subscriptions.clear();
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
					await this.#openAndReplay(this.activeSession);
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
			// Subscribe to the event stream immediately so the
			// session list's running pips light up without having
			// to open a session first. Without this, busySessions
			// stays empty until the user opens a session.
			this.#ensureSubscribed(workspace, ide);
			void this.#loadModelSettings();
			void this.loadScmStatus();
			void this.#loadRunningSessions();
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
		this.scmStatus = null;
		// Opening the folder acknowledges its "finished" dot. The
		// busy set stays — it spans all folders in the workspace.
		if (this.folderAttention.has(path)) {
			const next = new Set(this.folderAttention);
			next.delete(path);
			this.folderAttention = next;
		}
		this.error = null;
		this.loadingSessions = true;
		try {
			this.sessions = await this.#loadSessions();
			void this.loadScmStatus();
			void this.#loadRunningSessions();
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

	/** Back out of the active workspace to the switcher. Refreshes
	 * the workspace list so the live/stopped flags are current — a
	 * list fetched hours ago can show closed workspaces as running. */
	closeWorkspace(): void {
		void this.loadWorkspaces();
		this.activeWorkspace = null;
		this.activeWorkspaceName = '';
		this.activeIde = '';
		this.folders = [];
		this.activeFolder = null;
		this.coderStatus = null;
		this.modelSettings = null;
		this.scmStatus = null;
		this.sessions = [];
		this.busySessions = new Set();
		this.folderAttention = new Set();
		this.#sessionFolder.clear();
		this.error = null;
		this.closeSession();
	}

	/** Best-effort read of the workspace's model/provider settings.
	 * An IDE build that predates the methods just leaves the
	 * provider card hidden. */
	async #loadModelSettings(): Promise<void> {
		if (!this.activeWorkspace) {
			return;
		}
		try {
			this.modelSettings = await this.#call<ModelSettings>(
				this.activeWorkspace,
				'coder_get_model_settings',
				{},
				this.activeIde,
			);
		} catch {
			this.modelSettings = null;
		}
	}

	/** Display label for a provider id (`null` = the implicit HF
	 * route). Falls back to the raw id for a stale entry. */
	providerLabel(id: string | null | undefined): string {
		if (!id) {
			return 'Hugging Face';
		}
		return this.modelSettings?.providers.find((p) => p.id === id)?.label || id;
	}

	/** Switch the workspace's active provider (`null` = Hugging
	 * Face). When the workspace is locked, the lock is rewritten to
	 * the new pick — same semantics as the desktop picker, where a
	 * locked save interprets `active_provider` as the lock's value
	 * and leaves the global default untouched. */
	async setProvider(id: string | null): Promise<void> {
		const settings = this.modelSettings;
		if (!this.activeWorkspace || !settings) {
			return;
		}
		this.savingProvider = true;
		try {
			const next: ModelSettings = { ...settings, active_provider: id };
			if (settings.provider_lock) {
				next.provider_lock = id ? { kind: 'user', id } : { kind: 'hf' };
			}
			await this.#call(this.activeWorkspace, 'coder_set_model_settings', { settings: next }, this.activeIde);
			this.modelSettings = next;
		} catch (e) {
			this.error = e instanceof Error ? e.message : String(e);
		} finally {
			this.savingProvider = false;
		}
	}

	/** Toggle the per-workspace provider lock. Locking pins the
	 * current provider; unlocking makes the workspace follow (and
	 * writes) the global default — desktop-picker semantics. */
	/** Fetch the active folder's SCM status (branch + changed
	 * files). Best-effort: an IDE build that predates the method
	 * leaves the card hidden. */
	async loadScmStatus(): Promise<void> {
		if (!this.activeWorkspace || !this.activeFolder) {
			return;
		}
		this.loadingScm = true;
		try {
			this.scmStatus = await this.#call<ScmStatus>(
				this.activeWorkspace,
				'workspace_scm_status',
				{ folder: this.activeFolder },
				this.activeIde,
			);
		} catch {
			this.scmStatus = null;
		} finally {
			this.loadingScm = false;
		}
	}

	/** Branch review (vs the default branch) for the open session's
	 * folder — `null` when closed. `base_ref === null` after a load
	 * means "nothing to review against" (on the default branch,
	 * detached HEAD, or no remote). */
	review = $state<{
		folder: string;
		base_ref: string | null;
		files: { path: string; status: string }[];
		diff: string;
	} | null>(null);
	loadingReview = $state(false);

	/** Fetch the review payload for `folder` (a worktree root or a
	 * bound folder path) and open the review view. */
	async loadReview(folder: string): Promise<void> {
		if (!this.activeWorkspace || this.loadingReview) {
			return;
		}
		this.loadingReview = true;
		try {
			const r = await this.#call<{
				base_ref: string | null;
				files?: { path: string; status: string }[];
				diff?: string;
			}>(this.activeWorkspace, 'workspace_scm_review', { folder }, this.activeIde);
			this.review = {
				folder,
				base_ref: r.base_ref,
				files: r.files ?? [],
				diff: r.diff ?? '',
			};
		} catch (e) {
			this.error = e instanceof Error ? e.message : String(e);
		} finally {
			this.loadingReview = false;
		}
	}

	closeReview(): void {
		this.review = null;
	}

	/** Ask the fast model for a one-line commit subject from the
	 * active folder's `git diff HEAD`. Mirrors the desktop's
	 * sparkle button. Returns the suggestion; the caller decides
	 * whether to auto-fill. */
	async suggestCommitMessage(): Promise<string | null> {
		if (!this.activeWorkspace || !this.activeFolder) {
			return null;
		}
		try {
			const result = await this.#call<{ message: string }>(
				this.activeWorkspace,
				'workspace_scm_suggest_message',
				{ folder: this.activeFolder },
				this.activeIde,
			);
			return result.message;
		} catch (e) {
			this.error = e instanceof Error ? e.message : String(e);
			return null;
		}
	}

	/** Commit the active folder's staged + unstaged changes. When
	 * `message` is empty, the backend auto-suggests one from the diff
	 * (same fast-model path as the desktop's sparkle button). */
	async commit(message: string, amend = false): Promise<ScmCommitResult | null> {
		if (!this.activeWorkspace || !this.activeFolder) {
			return null;
		}
		this.committing = true;
		try {
			const result = await this.#call<ScmCommitResult>(
				this.activeWorkspace,
				'workspace_scm_commit',
				{ message, amend, folder: this.activeFolder },
				this.activeIde,
			);
			// Refresh status after commit.
			void this.loadScmStatus();
			return result;
		} catch (e) {
			this.error = e instanceof Error ? e.message : String(e);
			return null;
		} finally {
			this.committing = false;
		}
	}

	/** Sync the active folder's branch with upstream — same
	 * context-aware logic as the desktop's "Sync Changes" button:
	 * if behind, pull (rebase) first; if ahead (or after the pull),
	 * push. A diverged branch only pulls on the first click — the
	 * user reviews the rebased history before the next click pushes.
	 * The IDE auto-fetches on its own; this is the manual gesture. */
	async scmSync(): Promise<void> {
		if (!this.activeWorkspace || !this.activeFolder || !this.scmStatus) {
			return;
		}
		this.scmBusy = true;
		this.error = null;
		try {
			const branch = await this.#call<{ ahead: number; behind: number }>(
				this.activeWorkspace,
				'workspace_scm_sync',
				{ folder: this.activeFolder },
				this.activeIde,
			);
			if (this.scmStatus?.branch) {
				this.scmStatus.branch.ahead = branch.ahead;
				this.scmStatus.branch.behind = branch.behind;
			}
			void this.loadScmStatus();
		} catch (e) {
			this.error = e instanceof Error ? e.message : String(e);
		} finally {
			this.scmBusy = false;
		}
	}

	/** Switch the active folder's working tree to a local branch by
	 * name — the "back to main" chip. Errors (dirty tree, unknown
	 * branch) surface verbatim from git. */
	async scmSwitchBranch(name: string): Promise<void> {
		if (!this.activeWorkspace || !this.activeFolder || !name) {
			return;
		}
		this.scmBusy = true;
		this.error = null;
		try {
			await this.#call(
				this.activeWorkspace,
				'workspace_scm_switch_branch',
				{ name, folder: this.activeFolder },
				this.activeIde,
			);
			await this.loadScmStatus();
		} catch (e) {
			this.error = e instanceof Error ? e.message : String(e);
		} finally {
			this.scmBusy = false;
		}
	}

	async setProviderLock(locked: boolean): Promise<void> {
		const settings = this.modelSettings;
		if (!this.activeWorkspace || !settings) {
			return;
		}
		this.savingProvider = true;
		try {
			const active = settings.active_provider ?? null;
			const lock: ProviderLock | null = locked ? (active ? { kind: 'user', id: active } : { kind: 'hf' }) : null;
			const next: ModelSettings = { ...settings, provider_lock: lock };
			await this.#call(this.activeWorkspace, 'coder_set_model_settings', { settings: next }, this.activeIde);
			this.modelSettings = next;
		} catch (e) {
			this.error = e instanceof Error ? e.message : String(e);
		} finally {
			this.savingProvider = false;
		}
	}

	/** Set the standard model for the Hugging Face route (slug, e.g.
	 * `moonshotai/Kimi-K3` or `owner/name:provider`). Round-trips the
	 * whole settings payload, so the desktop picker and the runner
	 * see it exactly like a desktop save. Empty resets to the
	 * built-in default. HF route only — user providers carry their
	 * model in their own config, which stays desktop-edited. */
	async setStandardModel(slug: string): Promise<void> {
		const settings = this.modelSettings;
		if (!this.activeWorkspace || !settings) {
			return;
		}
		this.savingProvider = true;
		try {
			const next: ModelSettings = { ...settings, standard_model: slug.trim() };
			await this.#call(this.activeWorkspace, 'coder_set_model_settings', { settings: next }, this.activeIde);
			this.modelSettings = next;
			// Re-read so `resolved_standard_model` reflects the runner's
			// fallback chain (e.g. an emptied slug resolving to the
			// built-in default).
			void this.#loadModelSettings();
		} catch (e) {
			this.error = e instanceof Error ? e.message : String(e);
		} finally {
			this.savingProvider = false;
		}
	}

	/** Context-window cap for the current standard model, in
	 * **thousands of tokens (k)**, or null when the catalog window is
	 * used directly. Reads `context_window_overrides[resolved_standard_model]`
	 * (the runner falls back to the suffix-stripped base too, but the
	 * phone edits the resolved slug). */
	get contextCap(): { slug: string; capK: number | null } | null {
		const s = this.modelSettings;
		const slug = s?.resolved_standard_model;
		if (!s || !slug) {
			return null;
		}
		const tokens = s.context_window_overrides?.[slug];
		return { slug, capK: tokens && tokens > 0 ? Math.round(tokens / 1000) : null };
	}

	/** Set or clear the context-window cap for the current standard
	 * model, in **k** (`500` → a 500k cap). `capK <= 0` / null clears
	 * the override (back to the catalog window). Round-trips through
	 * `coder_set_model_settings` so the desktop picker and the runner
	 * both pick it up. */
	async setContextCap(capK: number | null): Promise<void> {
		const settings = this.modelSettings;
		const slug = settings?.resolved_standard_model;
		if (!this.activeWorkspace || !settings || !slug) {
			return;
		}
		this.savingProvider = true;
		try {
			const overrides = { ...settings.context_window_overrides };
			if (capK && capK > 0) {
				overrides[slug] = Math.round(capK) * 1000;
			} else {
				delete overrides[slug];
			}
			const next: ModelSettings = { ...settings, context_window_overrides: overrides };
			await this.#call(this.activeWorkspace, 'coder_set_model_settings', { settings: next }, this.activeIde);
			this.modelSettings = next;
		} catch (e) {
			this.error = e instanceof Error ? e.message : String(e);
		} finally {
			this.savingProvider = false;
		}
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

	/** Create a coordinator session (ADR 0030) — an orchestrator
	 * that spawns and manages worker agents in git worktrees. Can't
	 * edit files itself; delegates each task to a worker. */
	async newCoordinatorSession(): Promise<void> {
		if (!this.activeWorkspace) {
			return;
		}
		try {
			const summary = await this.#call<SessionSummary>(
				this.activeWorkspace,
				'coder_new_coordinator_session',
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

	/** Open a session: subscribe to live events, then observe-open on
	 * the backend — the transcript replay rides in the RPC response
	 * (so it can't race the subscription, and the desktop's own
	 * session view is never touched). */
	async openSession(id: string): Promise<void> {
		if (!this.activeWorkspace) {
			return;
		}
		this.activeSession = id;
		this.rows = [];
		this.busy = false;
		this.awaitingInput = false;
		this.pendingPrompt = null;
		this.hasMoreHistory = false;
		this.#oldestEventOrdinal = 0;
		this.loadingOlder = false;
		this.error = null;
		try {
			this.#ensureSubscribed(this.activeWorkspace, this.activeIde);
			await this.#openAndReplay(id);
		} catch (e) {
			this.error = e instanceof Error ? e.message : String(e);
		}
	}

	/** How many events the open / older-page calls ask for. Sized so
	 * the phone renders quickly (its render window starts at 50
	 * rows) while one page of scrolling feels instant. */
	static readonly HISTORY_PAGE_EVENTS = 120;

	/** Observe-open `id` and reduce the returned replay into rows.
	 * The trailing `turn_complete` terminator clears `busy`; a
	 * still-streaming background turn re-asserts it via `in_flight`
	 * (mirrors the desktop's replay handling). The replay is
	 * windowed (`max_events`) so a long / image-heavy session
	 * doesn't ship its whole history over the WS up front — the
	 * `history_window_start` boundary event seeds `hasMoreHistory`. */
	async #openAndReplay(id: string): Promise<void> {
		if (!this.activeWorkspace) {
			return;
		}
		const observed = await this.#call<ObservedSession>(
			this.activeWorkspace,
			'coder_open_session',
			{ id, folder: this.activeFolder, max_events: CompanionState.HISTORY_PAGE_EVENTS },
			this.activeIde,
		);
		for (const event of observed.events ?? []) {
			this.#onCoderEvent({ folder: this.activeFolder ?? '', session_id: id, event }, true);
		}
		if (observed.in_flight) {
			this.busy = true;
			// Re-derive a live-parked ask_user prompt: its replayed
			// tool_call set the prompt state, but the terminator
			// cleared it again.
			const pending = this.rows.find((r) => r.kind === 'ask_user' && !r.answered);
			if (pending && pending.kind === 'ask_user') {
				this.awaitingInput = true;
				this.pendingPrompt = { callId: pending.callId, questions: pending.questions };
			}
		}
	}

	/** Fetch the next-older page of the open session's transcript
	 * and prepend it. Called by the view when upward scroll has
	 * exhausted the locally-windowed rows. No-op when there's
	 * nothing older or a fetch is already in flight. */
	async loadOlderHistory(): Promise<void> {
		if (!this.activeWorkspace || !this.activeSession || !this.hasMoreHistory || this.loadingOlder) {
			return;
		}
		const sessionId = this.activeSession;
		this.loadingOlder = true;
		try {
			const page = await this.#call<HistoryWindow>(
				this.activeWorkspace,
				'coder_session_history_older',
				{
					id: sessionId,
					folder: this.activeFolder,
					before_event_ordinal: this.#oldestEventOrdinal,
					max_events: CompanionState.HISTORY_PAGE_EVENTS,
				},
				this.activeIde,
			);
			// Reduce the page's events into a throwaway buffer (the
			// reducer + helpers target `#rowsOverride` while set),
			// then prepend it in one assignment so the view's window
			// slice moves once and the scroll anchor holds position.
			const older: TranscriptRow[] = [];
			this.#rowsOverride = older;
			try {
				for (const event of page.events ?? []) {
					this.#onCoderEvent({ folder: this.activeFolder ?? '', session_id: sessionId, event }, true);
				}
			} finally {
				this.#rowsOverride = null;
			}
			if (older.length > 0) {
				this.rows = [...older, ...this.rows];
			}
			this.lastOlderPageRows = older.length;
			this.hasMoreHistory = page.has_more;
			this.#oldestEventOrdinal = page.before_event_ordinal;
		} catch (e) {
			this.error = e instanceof Error ? e.message : String(e);
		} finally {
			this.loadingOlder = false;
		}
	}

	closeSession(): void {
		this.activeSession = null;
		this.review = null;
		this.rows = [];
		this.busy = false;
		this.awaitingInput = false;
		this.pendingPrompt = null;
		this.hasMoreHistory = false;
		this.#oldestEventOrdinal = 0;
		this.loadingOlder = false;
	}

	/** Rename the session the phone has open. The backend persists
	 * a `TitleUpdate` and broadcasts `session_title_updated`, which
	 * both the desktop panel and this phone's own subscription apply
	 * — so the rename propagates to the IDE without a refresh. */
	async renameSession(title: string): Promise<void> {
		if (!this.activeWorkspace || !this.activeSession || !title.trim()) {
			return;
		}
		try {
			await this.#call(
				this.activeWorkspace,
				'coder_rename_session',
				{ id: this.activeSession, title: title.trim(), folder: this.activeFolder },
				this.activeIde,
			);
		} catch (e) {
			this.error = e instanceof Error ? e.message : String(e);
		}
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

	/** 0-based ordinal of a user row among all non-queued user rows
	 * — the backend's `User`-record count matches this (same walk
	 * as the desktop's `#userOrdinalForRow`). */
	#userOrdinal(rowId: string): number | null {
		let ordinal = 0;
		for (const row of this.rows) {
			if (row.kind !== 'user' || row.queued) {
				continue;
			}
			if (row.id === rowId) {
				return ordinal;
			}
			ordinal += 1;
		}
		return null;
	}

	/** Revert the open session to just before the user message with
	 * `rowId` (dropping it and everything after from disk), repaint
	 * the transcript from the truncated JSONL, and return the
	 * dropped prompt text so the caller can seed the composer
	 * ("edit & resend") or re-send it verbatim ("replay"). Refused
	 * by the backend mid-turn; the UI hides the affordance while
	 * busy. */
	async revertToMessage(rowId: string): Promise<string | null> {
		if (!this.activeWorkspace || !this.activeSession || this.busy) {
			return null;
		}
		const ordinal = this.#userOrdinal(rowId);
		if (ordinal === null) {
			return null;
		}
		const sessionId = this.activeSession;
		try {
			const reverted = await this.#call<{ text: string }>(
				this.activeWorkspace,
				'coder_revert_to_message',
				{ session_id: sessionId, user_ordinal: ordinal },
				this.activeIde,
			);
			// Repaint from the truncated JSONL.
			this.rows = [];
			await this.#openAndReplay(sessionId);
			return reverted.text;
		} catch (e) {
			this.error = e instanceof Error ? e.message : String(e);
			return null;
		}
	}

	/** Replay from the user message with `rowId`: revert to just
	 * before it, then immediately re-send the dropped prompt
	 * verbatim — "re-run this turn". */
	async replayFromMessage(rowId: string): Promise<void> {
		const text = await this.revertToMessage(rowId);
		if (text !== null && text.trim()) {
			await this.sendPrompt(text);
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

	/** Un-queue a still-queued steer: pop it out of the backend's
	 * pending-steer queue and return its text so the caller can seed
	 * the composer ("edit before it lands"). The matching
	 * `steer_drained` event removes the queued row over the event
	 * channel. `null` when the steer already drained (too late) —
	 * the caller leaves the draft alone. Session-targeted by id (the
	 * session the phone has open), like send/abort. */
	async unqueueSteer(rowId: string): Promise<string | null> {
		if (!this.activeWorkspace || !this.activeSession) {
			return null;
		}
		try {
			const res = await this.#call<{ text: string | null }>(
				this.activeWorkspace,
				'coder_unqueue_steer',
				{ session_id: this.activeSession, id: rowId },
				this.activeIde,
			);
			return res.text;
		} catch (e) {
			this.error = e instanceof Error ? e.message : String(e);
			return null;
		}
	}

	/** "Go now" on a queued steer: cancel the running turn so the
	 * spawn loop drains this steer into a fresh turn immediately
	 * instead of waiting for the current turn to settle. The backend
	 * emits `steer_drained` + a fresh `user_message` over the event
	 * channel, so the transcript updates itself. A stale tap (the
	 * runner already drained it) is a silent no-op. */
	async drainSteerNow(rowId: string): Promise<void> {
		if (!this.activeWorkspace || !this.activeSession) {
			return;
		}
		try {
			await this.#call(
				this.activeWorkspace,
				'coder_drain_steer_now',
				{ session_id: this.activeSession, id: rowId },
				this.activeIde,
			);
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
		if (!this.#socket || !this.connection) {
			return;
		}
		const key = `${ide}\u0000${workspace}`;
		if (this.#subscriptions.has(key)) {
			return;
		}
		if (this.#subscriptions.size === 0) {
			this.#socket.onEvent((raw) => this.#onCoderEvent(raw));
		}
		this.#socket.subscribe(this.connection.token, workspace, ide);
		this.#subscriptions.add(key);
	}

	/** Reduce a coder event envelope onto the transcript rows. */
	/** Toggle a session's busy state in the `busySessions` set.
	 * Replaces the set so Svelte reactivity fires. */
	#markBusy(sid: string, busy: boolean): void {
		const next = new Set(this.busySessions);
		if (busy) {
			next.add(sid);
		} else {
			next.delete(sid);
		}
		if (next.size !== this.busySessions.size || [...next].some((s) => !this.busySessions.has(s))) {
			this.busySessions = next;
		}
	}

	/** Seed the session list's "running" pips from the backend's
	 * authoritative set. The pip is otherwise event-driven and
	 * misses sessions already in flight when the phone subscribes,
	 * and queued steers (which emit no live `user_message`). Called
	 * on workspace open and each folder's session-list refresh. */
	async #loadRunningSessions(): Promise<void> {
		if (!this.activeWorkspace) {
			return;
		}
		try {
			const running = await this.#call<string[]>(
				this.activeWorkspace,
				'coder_running_sessions',
				{ folder: this.activeFolder },
				this.activeIde,
			);
			this.busySessions = new Set(running);
		} catch {
			// An IDE build that predates the method leaves the pip
			// event-driven (degrades to the old behaviour).
		}
	}

	#onCoderEvent(raw: unknown, fromReplay = false): void {
		// eslint-disable-next-line typescript-eslint/no-unsafe-type-assertion
		const envelope = (raw ?? {}) as CoderEventEnvelope;
		const ev = envelope.event;
		if (!ev) {
			return;
		}
		// Cross-carrier filter: subscriptions accumulate for the
		// socket's lifetime (previously-visited workspaces keep
		// streaming), and folder paths / workspace slugs collide
		// across hosts — so events tagged for a different IDE or
		// workspace than the one the phone is looking at must not
		// touch pips, attention dots or the transcript. Untagged
		// envelopes (older bridge, local replay reduction) pass.
		if (typeof envelope.ide === 'string' && this.activeWorkspace !== null) {
			if (envelope.ide !== this.activeIde || envelope.workspace !== this.activeWorkspace) {
				return;
			}
		}

		// --- Per-session busy tracking (for the session list's
		// running pip and the project chips' folder pips).
		// Processed for *all* sessions before the active-session
		// filter below, so a background session's pip stays lit
		// while the user browses the list. The set is seeded from
		// the backend on load and then kept current by *live*
		// events; replay only asserts it via the batch's `in_flight`
		// flag. A live completion in another folder lights that
		// project chip's "finished" dot — a replayed one never does.
		const eventSid = envelope.session_id;
		const eventFolder = envelope.folder || this.activeFolder || '';
		// Title updates apply to the sessions list (and the open
		// header, which reads from it) regardless of which session
		// is active — processed before the active-session filter so
		// a rename from the desktop, or of a background session,
		// still lands.
		if (ev.kind === 'session_title_updated') {
			this.#updateSessionTitle(str(ev, 'id'), str(ev, 'title'));
			return;
		}
		// Don't let a *replayed* user_message / turn flip the pip:
		// a windowed replay pairs its user_message with a trailing
		// turn_complete terminator, so a settled session nets out —
		// but a queued steer replays a user_message with no
		// terminator, and would leave the pip stuck on. The pip is
		// driven by live events + the backend-seeded set; replay
		// only lights it via the batch's `in_flight` flag.
		if (eventSid) {
			if (ev.kind === 'replay' && bool(ev, 'in_flight')) {
				this.#markBusy(eventSid, true);
				this.#sessionFolder.set(eventSid, eventFolder);
			} else if (!fromReplay && ev.kind === 'user_message') {
				this.#markBusy(eventSid, true);
				this.#sessionFolder.set(eventSid, eventFolder);
			} else if (ev.kind === 'turn_complete' || ev.kind === 'aborted' || ev.kind === 'error') {
				this.#markBusy(eventSid, false);
				// A live turn_complete in a folder the phone isn't
				// looking at lights that project chip's "finished"
				// dot (cleared when the user opens the folder).
				if (!fromReplay && ev.kind === 'turn_complete' && eventFolder && eventFolder !== this.activeFolder) {
					if (!this.folderAttention.has(eventFolder)) {
						const next = new Set(this.folderAttention);
						next.add(eventFolder);
						this.folderAttention = next;
					}
				}
			}
		}

		// Only render transcript events for the session the phone
		// has open.
		if (this.activeSession && eventSid && eventSid !== this.activeSession) {
			return;
		}
		// A `replay` batch packs a whole session's historic events
		// into one envelope. Unpack and feed each inner event back
		// through this reducer.
		if (ev.kind === 'replay') {
			const inner = ev.events;
			if (Array.isArray(inner)) {
				for (const e of inner) {
					this.#onCoderEvent({ ...envelope, event: e }, true);
				}
			}
			if (bool(ev, 'in_flight')) {
				this.busy = true;
			}
			return;
		}
		if (typeof ev.kind !== 'string') {
			return;
		}
		// The windowed-replay boundary event: records where the
		// visible window starts and flags that older history exists.
		if (ev.kind === 'history_window_start') {
			this.hasMoreHistory = true;
			this.#oldestEventOrdinal = num(ev, 'before_event_ordinal');
			return;
		}
		const rows = this.#rowsOverride ?? this.rows;
		switch (ev.kind) {
			case 'user_message': {
				// A queued steer arrives as a provisional bubble; a
				// drained one arrives as a fresh `queued: false` message
				// (new id) appended at the bottom. Either way it's a
				// plain append — ids never collide with an existing row.
				rows.push({
					kind: 'user',
					id: str(ev, 'id'),
					text: str(ev, 'text'),
					queued: bool(ev, 'queued'),
					fromCoordinator: bool(ev, 'from_coordinator'),
				});
				break;
			}
			case 'steer_drained': {
				// Remove the provisional queued placeholder. On a real
				// drain the runner follows with a fresh `user_message`
				// appended at the bottom (after the answer that was
				// already streaming); on an un-queue nothing follows.
				const idx = rows.findLastIndex((r) => r.kind === 'user' && r.id === str(ev, 'id'));
				if (idx !== -1) {
					rows.splice(idx, 1);
				}
				break;
			}
			case 'assistant_message_start':
				this.busy = true;
				rows.push({ kind: 'assistant', id: str(ev, 'id'), text: '', thinking: '' });
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
				const callId = str(ev, 'id');
				// Idempotent by id: observing a session whose turn is
				// still running replays the persisted assistant record
				// (the whole tool-call batch, written before the first
				// tool dispatches) and the live turn then emits its own
				// `tool_call` for the calls that hadn't started yet. A
				// second row for the same id would trip the keyed
				// `{#each}`'s `each_key_duplicate`.
				const known = rows.findLast((r) => (r.kind === 'tool' || r.kind === 'ask_user') && r.id === callId);
				// ask_user gets its own row kind so the UI can render
				// the interactive prompt.
				if (name === 'ask_user') {
					const questions = parseAskUserArgs(args);
					if (known?.kind !== 'ask_user') {
						rows.push({
							kind: 'ask_user',
							id: callId,
							callId,
							questions,
							answered: false,
						});
					}
					// An already-answered prompt must not re-park the
					// composer when its `tool_call` is re-observed.
					if (known?.kind !== 'ask_user' || !known.answered) {
						this.awaitingInput = true;
						this.pendingPrompt = { callId, questions };
					}
				} else if (known?.kind === 'tool') {
					known.args = argsStr;
				} else {
					rows.push({
						kind: 'tool',
						id: callId,
						name,
						args: argsStr,
						result: '',
						images: [],
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
				const askRow = rows.find((r) => r.kind === 'ask_user' && r.callId === id);
				if (askRow && askRow.kind === 'ask_user') {
					this.awaitingInput = false;
					this.pendingPrompt = null;
				} else {
					const result = ev['result'];
					const images = toolImagesOf(result);
					const stripped = withoutToolImages(result);
					const resultStr = typeof stripped === 'string' ? stripped : JSON.stringify(stripped);
					this.#setToolResult(id, resultStr, isError ? 'error' : 'done', images);
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
					// Update the existing tokens row in place rather
					// than appending a new one each time — the coder
					// emits these frequently during a turn and each
					// would otherwise become its own transcript row.
					const existing = rows.findLast((r) => r.kind === 'tokens');
					if (existing && existing.kind === 'tokens') {
						existing.total = total;
						existing.contextWindow = ctx;
					} else {
						rows.push({
							kind: 'tokens',
							id: nextRowId('tok'),
							total,
							contextWindow: ctx,
						});
					}
				}
				break;
			}
			case 'turn_diff': {
				const files = ev['files'];
				const diff = str(ev, 'diff');
				const fileList = Array.isArray(files) ? files.map(String) : [];
				if (fileList.length > 0 || diff) {
					rows.push({
						kind: 'diff',
						id: nextRowId('diff'),
						files: fileList,
						diff,
					});
				}
				break;
			}
			case 'compaction_started':
				rows.push({
					kind: 'compaction',
					id: nextRowId('comp'),
					summary: '',
					done: false,
				});
				break;
			case 'compaction_complete': {
				const summary = str(ev, 'summary');
				const row = rows.findLast((r) => r.kind === 'compaction' && !r.done);
				if (row && row.kind === 'compaction') {
					row.summary = summary;
					row.done = true;
				}
				break;
			}
			case 'subagent_spawned':
				rows.push({
					kind: 'subagent',
					id: `sub-${str(ev, 'subagent_id')}`,
					subagentId: str(ev, 'subagent_id'),
					folder: str(ev, 'target_folder'),
					finished: false,
					detached: bool(ev, 'detached'),
				});
				break;
			case 'subagent_finished': {
				const sid = str(ev, 'subagent_id');
				const row = rows.findLast((r) => r.kind === 'subagent' && r.subagentId === sid);
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
		const rows = this.#rowsOverride ?? this.rows;
		// If id is empty, it's a thinking delta — append to the last
		// assistant row's thinking field.
		if (!id) {
			const row = rows.findLast((r) => r.kind === 'assistant');
			if (row && row.kind === 'assistant') {
				row.thinking += thinkingDelta;
			}
			return;
		}
		const row = rows.find((r) => r.kind === 'assistant' && r.id === id);
		if (row && row.kind === 'assistant') {
			row.text += delta;
			row.thinking += thinkingDelta;
		} else {
			rows.push({ kind: 'assistant', id, text: delta, thinking: thinkingDelta });
		}
	}

	#setAssistant(id: string, text: string, thinking: string): void {
		const rows = this.#rowsOverride ?? this.rows;
		const row = rows.find((r) => r.kind === 'assistant' && r.id === id);
		if (row && row.kind === 'assistant') {
			row.text = text;
			if (thinking) {
				row.thinking = thinking;
			}
		}
	}

	#setToolResult(id: string, result: string, status: 'done' | 'error', images: ToolImage[] = []): void {
		const rows = this.#rowsOverride ?? this.rows;
		const row = rows.find((r) => r.kind === 'tool' && r.id === id);
		if (row && row.kind === 'tool') {
			row.result = result;
			row.images = images;
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
