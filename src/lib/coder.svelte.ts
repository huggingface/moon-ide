//! Reactive state for the Coder panel.
//!
//! Phase 6.1 surface: device-flow sign-in, single in-memory session,
//! send / abort, **streaming assistant messages**. The panel rebuilds
//! its message list from the `coder:event` Tauri stream — there's
//! no persistence layer behind it yet (lands in 6.3). A page reload
//! therefore loses the visible transcript; the loop's own session
//! memory survives because it lives in the Rust process.
//!
//! Streaming wire shape (mirrors `moon_coder::CoderEvent`):
//! `assistant_message_start { id }` → N × `assistant_message_delta
//! { id, delta }` → `assistant_message_end { id, text }`. The end
//! event carries the canonical full content; we replace the
//! accumulated string with it on close so any drift between the
//! deltas and the final assembly heals.
//!
//! See `specs/coder.md` and `specs/test-plans/0039-coder-skeleton.md`.

import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import { ipc } from './ipc';
import {
	formatError,
	type CoderEvent,
	type CoderEventEnvelope,
	type CoderSessionSummary,
	type CoderStatus,
	type DeviceCode,
	type HfIdentity,
	type SubagentMode,
} from './protocol';
import { rightPanel } from './rightPanel.svelte';

const CODER_EVENT_CHANNEL = 'coder:event';

/** One row rendered in the panel transcript. The `kind` matches the
 *  loop event that produced it; the `id` is stable so the runner's
 *  `tool_call` → `tool_result` pair update the same DOM node when
 *  the tool finishes.
 *
 *  Assistant rows track an optional `thinking` trace alongside
 *  `text`. `thinkingOpen` controls the disclosure state: open while
 *  the message is still streaming so the user can watch reasoning
 *  land, auto-collapsed on `assistant_message_end`.
 *
 *  Tool rows carry `startedAt` (epoch ms, set on `tool_call`) and
 *  `durationMs` (set on `tool_result`). The panel uses the first to
 *  drive a live ticking elapsed counter while the tool runs and the
 *  second to display the precise elapsed time once the call settles
 *  — useful for spotting slow tools (multi-second `bash` tail,
 *  multi-megabyte `read_file`) at a glance. */
export type CoderRow =
	| { kind: 'user'; id: string; text: string }
	| { kind: 'assistant'; id: string; text: string; thinking: string; thinkingOpen: boolean }
	| {
			kind: 'tool';
			id: string;
			name: string;
			args: unknown;
			result: unknown;
			hasResult: boolean;
			isError: boolean;
			startedAt: number;
			durationMs: number | null;
	  }
	| { kind: 'error'; id: string; text: string }
	| { kind: 'aborted'; id: string };

/** Which view of the Coder panel is mounted. `'list'` shows the
 *  sessions list (mirrors the Slack panel's "← Sessions" gesture);
 *  `'session'` shows an active session's transcript + composer;
 *  `'subagent'` is the pop-out for a single sub-agent's full
 *  transcript — back-arrow returns to the parent's session. */
export type CoderView = 'list' | 'session' | 'subagent';

/** Status of one sub-agent currently visible in the parent's
 *  transcript. Drives the collapsed card under each `spawn_subagent`
 *  tool row: `running` while events stream in, `done` /
 *  `error` / `aborted` once `subagent_finished` lands. */
export type SubagentStatus = 'running' | 'done' | 'error';

/** Summary card displayed inline under a `spawn_subagent` tool
 *  call in the parent's transcript. Keyed by `toolCallId` so the
 *  card lookup matches the tool row's stable id. */
export type SubagentSummary = {
	id: string;
	toolCallId: string;
	targetFolder: string;
	mode: SubagentMode;
	status: SubagentStatus;
	resultPreview: string | null;
	tokensUsedEstimate: number;
	subSessionId: string | null;
};

/** Full transcript for one sub-agent, populated incrementally
 *  from `subagent_event` arrivals. The pop-out view (`view ===
 *  'subagent'`) renders these rows with the same components the
 *  parent transcript uses. In-memory only — closing + reopening
 *  the parent session today does not reload prior sub-agent
 *  transcripts from disk (lands when the sub-agent JSONL replay
 *  hits the frontend). */
export type SubagentTranscript = {
	id: string;
	toolCallId: string;
	mode: SubagentMode;
	targetFolder: string;
	rows: CoderRow[];
};

/** One piece of editor context the user has attached to the
 *  composer via the Ctrl+L "add to chat" gesture (mirrors
 *  Cursor's `@file:line-line` chips). The text is captured at
 *  attach time so a follow-up edit to the file doesn't change
 *  what the agent sees — the user pinned a snapshot, not a
 *  reference.
 *
 *  Each attachment has a stable `token` (`@path:start-end`) that
 *  also lives inline in the composer textarea — same shape
 *  Cursor uses. Send-time formatting reads this token to decide
 *  the order of attachments in the trailing `<context>` block,
 *  and the panel's `×` button strips matching tokens out of the
 *  draft so the chip and the inline reference always agree. */
export type ComposerAttachment = {
	id: string;
	token: string;
	path: string;
	startLine: number;
	endLine: number;
	text: string;
};

/** Per-bound-folder UI state. One instance per folder we've ever
 *  routed an event for; lazily created via
 *  [`CoderPanelState.bucketFor`]. Held in `byFolder` map keyed by
 *  the folder's absolute path (matches `WorkspaceFolder.path`).
 *
 *  Per-folder so that a turn running in folder X keeps streaming
 *  rows / busy / sub-agent updates into X's bucket while the user
 *  is browsing folder Y — switching active folder swaps which
 *  bucket the panel reads from, no IPC, no state loss. Per the
 *  multi-session decision: composer draft and attachments live
 *  here too, so each project's typed-but-unsent prose survives a
 *  folder hop. */
class FolderViewState {
	rows = $state<CoderRow[]>([]);
	busy = $state(false);
	activeSession = $state<CoderSessionSummary | null>(null);
	view = $state<CoderView>('session');
	viewSubagentId = $state<string | null>(null);
	subagentSummaries = $state<Map<string, SubagentSummary>>(new Map());
	subagentTranscripts = $state<Map<string, SubagentTranscript>>(new Map());
	sessions = $state<CoderSessionSummary[] | null>(null);
	draft = $state('');
	attachments = $state<ComposerAttachment[]>([]);
	/** Latest token usage report from the parent loop. `null`
	 *  before the first turn; populated from `token_usage` events
	 *  and used by [`ContextRing`] in the panel header. */
	tokenUsage = $state<TokenUsageState | null>(null);
	/** Auto-compaction status. `null` when nothing is in flight;
	 *  `{ phase: 'running', ... }` while the fast-model summary
	 *  call is out; `{ phase: 'done', ... }` after `compaction_complete`
	 *  lands so the UI can render the disclosure with the summary
	 *  body until the next compaction overwrites it. */
	compaction = $state<CompactionState | null>(null);
}

/**
 * One row of the rolling token-usage report.
 *
 * `prompt` is the load-bearing field — it tells the user (and the
 * compaction trigger) how much of the model's context window the
 * **next** round-trip is going to take to fit history into. The
 * other numbers are informational; the ring uses
 * `prompt / contextWindow` for the fill arc.
 */
export type TokenUsageState = {
	prompt: number;
	completion: number;
	total: number;
	contextWindow: number;
	/** `'provider'` when the figures came from the OpenAI-compatible
	 *  streaming `usage` chunk; `'estimate'` when we fell back to a
	 *  bytes/4 approximation. The ring tints identically; the
	 *  tooltip prefixes a `≈` for `'estimate'`. */
	source: 'provider' | 'estimate';
};

/**
 * Compaction event for the panel.
 *
 * `'running'` shows a "compacting…" pip in the panel header while
 * the fast-model summary call is in flight; `'done'` flips the pip
 * to a disclosure that, when expanded, reveals the synthetic
 * summary the agent now sees in place of the older middle of the
 * history.
 */
export type CompactionState =
	| { phase: 'running'; messagesCompacted: number }
	| { phase: 'done'; messagesCompacted: number; summary: string; promptTokensAfter: number };

/** Sentinel folder key used when no workspace folder is active.
 *  Pre-binding the agent panel still lets the user start typing
 *  into the composer; the draft stays under this key until a
 *  folder gets bound (at which point fresh state spins up for
 *  that folder). Empty string is convenient because it can never
 *  collide with a real absolute path. */
const NO_FOLDER_KEY = '';

class CoderPanelState {
	/** Whether the right-side slot is currently mounted with the
	 *  coder surface. Derived from the shared `rightPanel.kind` —
	 *  chat and coder share one slot. */
	get panelVisible(): boolean {
		return rightPanel.kind === 'coder';
	}

	/** Latest `coder_status`. `null` before the first call. Global
	 *  (auth + bash_target are workspace-wide concepts). */
	status = $state<CoderStatus | null>(null);

	/** Active device-flow code, while the connect modal is open.
	 *  Global — the device flow lives at the auth layer, not per
	 *  project. */
	deviceCode = $state<DeviceCode | null>(null);

	/** UI flag while `coder_start_device_flow` is in flight. */
	startingFlow = $state(false);

	/** UI flag while we're polling the token endpoint. */
	awaitingApproval = $state(false);

	/** Latest sign-in error (device-flow expired, denied, network). */
	signInError = $state<string | null>(null);

	/** Cached "Bound folders" descriptions populated by
	 *  `folder_summary_ready` events. Folder absolute path →
	 *  description text. Used by the project-bar tooltip
	 *  (follow-up plan) and the sub-agent target picker preview.
	 *  Shared across folder buckets — descriptions are inherently
	 *  per-folder data, but every folder's UI may want to read any
	 *  folder's description. */
	folderDescriptions = $state<Map<string, string>>(new Map());

	/** Per-folder UI state buckets. Keyed by absolute path.
	 *  Lazily populated via [`bucketFor`]; never explicitly
	 *  removed (cheap, and a folder rebound after removal gets
	 *  the same bucket back, which is what the user expects).
	 *  The map itself is **not** `$state` — only the inner
	 *  `FolderViewState`'s `$state` fields drive component
	 *  re-renders, so we don't need to re-allocate the map on
	 *  every bucket access. */
	byFolder = new Map<string, FolderViewState>();

	/** Absolute path of the currently active workspace folder, or
	 *  `null` when none is bound. Updated externally via
	 *  [`setActiveFolder`] from `state.svelte.ts` whenever the
	 *  workspace switches active folder; `coder.svelte.ts` deliberately
	 *  does not import `state.svelte.ts` to avoid the cycle. The
	 *  `current` getter reads this so all per-folder accessors
	 *  re-run via Svelte's reactivity when the user switches
	 *  projects. */
	activeFolderPath = $state<string | null>(null);

	/** Convenience: forwards to `bucketFor(activeFolderPath)`.
	 *  Reading any per-folder field through this getter sets up
	 *  a reactivity dependency on `activeFolderPath`, so a folder
	 *  switch re-renders the panel against the new bucket. */
	get current(): FolderViewState {
		return this.bucketFor(this.activeFolderPath ?? NO_FOLDER_KEY);
	}

	/** Look up (and lazily create) the bucket for a specific folder.
	 *  Used by the event dispatcher to route incoming envelopes to
	 *  the right folder's UI state, even for folders the user has
	 *  never visited yet (a sub-agent spawn from another folder, a
	 *  background turn finishing while the user is elsewhere, etc.). */
	bucketFor(folder: string): FolderViewState {
		let entry = this.byFolder.get(folder);
		if (!entry) {
			entry = new FolderViewState();
			this.byFolder.set(folder, entry);
		}
		return entry;
	}

	/** Surface an "is anything running anywhere" flag for the
	 *  status-bar pip / global indicators. Walks every bucket; cheap
	 *  because we have at most a few folders bound at once. */
	get anyBusy(): boolean {
		for (const bucket of this.byFolder.values()) {
			if (bucket.busy) {
				return true;
			}
		}
		return false;
	}

	/** "Is the agent currently running a turn for this folder?"
	 *  Used by the project-bar to surface a pulsing pip when a
	 *  background turn is mid-flight in a folder the user isn't
	 *  currently viewing. Goes through `bucketFor` (not a raw
	 *  `byFolder.get`) so the read sets up reactivity on the
	 *  bucket's `busy` `$state`; the consequent lazy-create of
	 *  an empty bucket per bound folder is cheap (a handful of
	 *  null fields). */
	busyForFolder(folder: string): boolean {
		return this.bucketFor(folder).busy;
	}

	// Per-folder field forwards. Components keep reading
	// `coder.rows`, `coder.busy`, etc. unchanged — the indirection
	// through `current` keeps them on the right bucket while the
	// user navigates between projects.
	get sessions(): CoderSessionSummary[] | null {
		return this.current.sessions;
	}
	set sessions(value: CoderSessionSummary[] | null) {
		this.current.sessions = value;
	}

	get activeSession(): CoderSessionSummary | null {
		return this.current.activeSession;
	}
	set activeSession(value: CoderSessionSummary | null) {
		this.current.activeSession = value;
	}

	get view(): CoderView {
		return this.current.view;
	}
	set view(value: CoderView) {
		this.current.view = value;
	}

	get busy(): boolean {
		return this.current.busy;
	}
	set busy(value: boolean) {
		this.current.busy = value;
	}

	get rows(): CoderRow[] {
		return this.current.rows;
	}
	set rows(value: CoderRow[]) {
		this.current.rows = value;
	}

	get subagentSummaries(): Map<string, SubagentSummary> {
		return this.current.subagentSummaries;
	}
	set subagentSummaries(value: Map<string, SubagentSummary>) {
		this.current.subagentSummaries = value;
	}

	get subagentTranscripts(): Map<string, SubagentTranscript> {
		return this.current.subagentTranscripts;
	}
	set subagentTranscripts(value: Map<string, SubagentTranscript>) {
		this.current.subagentTranscripts = value;
	}

	get viewSubagentId(): string | null {
		return this.current.viewSubagentId;
	}
	set viewSubagentId(value: string | null) {
		this.current.viewSubagentId = value;
	}

	get draft(): string {
		return this.current.draft;
	}
	set draft(value: string) {
		this.current.draft = value;
	}

	get attachments(): ComposerAttachment[] {
		return this.current.attachments;
	}
	set attachments(value: ComposerAttachment[]) {
		this.current.attachments = value;
	}

	get tokenUsage(): TokenUsageState | null {
		return this.current.tokenUsage;
	}

	get compaction(): CompactionState | null {
		return this.current.compaction;
	}

	/** Counter the panel `$effect`s on to refocus the composer
	 *  after we mutate it programmatically (e.g. attaching a
	 *  selection from the editor via Ctrl+L). Increment to
	 *  request focus; the panel's effect compares the count
	 *  against its last-seen value and calls `.focus()` on
	 *  change. Bumping a counter (rather than firing an event)
	 *  keeps the side-effect inside Svelte's reactive graph. */
	composerFocusTick = $state(0);

	/** The composer's textarea node, bound by `CoderPanel.svelte`
	 *  on mount. Exposed here so methods that mutate the draft
	 *  (Ctrl+L attaches a token at the caret position; chip ×
	 *  scrubs a token out of the draft) can reach into the
	 *  textarea without prop-drilling a callback through the
	 *  panel's render tree. Cleared on unmount so a HMR'd panel
	 *  doesn't leave a dangling reference behind. */
	composerEl = $state<HTMLTextAreaElement | null>(null);

	/** Tauri-listener cleanup; one entry per `wireRuntime` call. */
	#unlisten: UnlistenFn[] = [];
	#runtimeWired = false;

	get signedIn(): boolean {
		return this.status?.signed_in ?? false;
	}

	get identity(): HfIdentity | null {
		return this.status?.identity ?? null;
	}

	/** Where the agent's `bash` tool will run for the active folder.
	 *  Surfaced in the panel header so the user knows whether
	 *  commands hit the host or the workspace container. `null`
	 *  before the first status probe lands or when the workspace has
	 *  no active folder. */
	get bashTarget(): 'host' | 'container' | null {
		return this.status?.bash_target ?? null;
	}

	togglePanel(): void {
		rightPanel.toggle('coder');
	}

	/** Open the panel + attach a file/range snapshot to the
	 *  composer. Bound to Ctrl+L from the editor. Idempotent on
	 *  duplicate attachments — pressing Ctrl+L twice on the same
	 *  selection adds the inline `@`-token *every* press (matches
	 *  Cursor — a second reference is a legitimate way to anchor
	 *  the model on the same code at a second spot in the prose),
	 *  but the chip strip dedupes by `path:start-end` so the
	 *  attachment list stays clean. Always lands focus in the
	 *  composer afterwards (via `composerFocusTick`). */
	addAttachmentFromSelection(snapshot: { path: string; startLine: number; endLine: number; text: string }): void {
		rightPanel.set('coder');
		this.view = 'session';
		const token = formatAttachmentToken(snapshot.path, snapshot.startLine, snapshot.endLine);
		const dup = this.attachments.find(
			(a) => a.path === snapshot.path && a.startLine === snapshot.startLine && a.endLine === snapshot.endLine,
		);
		if (!dup) {
			this.attachments = [
				...this.attachments,
				{
					id: `att-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`,
					token,
					path: snapshot.path,
					startLine: snapshot.startLine,
					endLine: snapshot.endLine,
					text: snapshot.text,
				},
			];
		}
		this.#insertTokenAtCaret(token);
		this.composerFocusTick = this.composerFocusTick + 1;
	}

	removeAttachment(id: string): void {
		const att = this.attachments.find((a) => a.id === id);
		if (!att) {
			return;
		}
		this.attachments = this.attachments.filter((a) => a.id !== id);
		// Strip every occurrence of the inline token (with at most
		// one trailing whitespace char) out of the draft. The user's
		// own typing might have nudged spacing around the token —
		// matching the token plus an optional `\s` keeps the most
		// common case clean without trying to be clever about
		// arbitrary punctuation.
		const pattern = new RegExp(`${escapeRegExp(att.token)}\\s?`, 'g');
		this.draft = this.draft.replace(pattern, '');
	}

	clearAttachments(): void {
		this.attachments = [];
	}

	/** Insert `token` at the textarea's caret position, with a
	 *  trailing space so the user can keep typing, and a leading
	 *  space when the previous character isn't already
	 *  whitespace. No-op when the textarea isn't mounted yet
	 *  (calling Ctrl+L before the panel ever rendered). */
	#insertTokenAtCaret(token: string): void {
		const ta = this.composerEl;
		if (!ta) {
			// Fallback: append at end of draft — better than dropping
			// the token on the floor. This only fires before the
			// panel mounts; in practice the focus tick mounts the
			// composer before we reach here anyway.
			const sep = this.draft.length > 0 && !/\s$/.test(this.draft) ? ' ' : '';
			this.draft = `${this.draft}${sep}${token} `;
			return;
		}
		const start = ta.selectionStart;
		const end = ta.selectionEnd;
		const before = this.draft.slice(0, start);
		const after = this.draft.slice(end);
		const needsLeading = before.length > 0 && !/\s$/.test(before);
		const needsTrailing = after.length === 0 || !/^\s/.test(after);
		const insertion = `${needsLeading ? ' ' : ''}${token}${needsTrailing ? ' ' : ''}`;
		this.draft = `${before}${insertion}${after}`;
		// Restore caret to just after the trailing space — the user
		// can keep typing without backing into the token. Defer to
		// a microtask so Svelte has flushed the bound `value` into
		// the DOM before we move the selection.
		const caret = before.length + insertion.length;
		queueMicrotask(() => {
			ta.selectionStart = caret;
			ta.selectionEnd = caret;
		});
	}

	/** Refresh the persisted sessions list. Best-effort: a
	 *  failure leaves the previous snapshot visible. */
	async refreshSessions(): Promise<void> {
		try {
			this.sessions = await ipc.coder.listSessions();
		} catch (err) {
			// eslint-disable-next-line no-console
			console.warn('coder: failed to list sessions', err);
		}
	}

	/** Open a persisted session by id. The backend emits
	 *  `session_loaded` + per-record replay events on the
	 *  `coder:event` channel; we just react to those, so this
	 *  method only needs to flip the panel into the session view. */
	async openSession(id: string): Promise<void> {
		this.rows = [];
		this.subagentSummaries = new Map();
		this.subagentTranscripts = new Map();
		this.viewSubagentId = null;
		this.busy = false;
		try {
			const summary = await ipc.coder.openSession(id);
			this.activeSession = summary;
			this.view = 'session';
		} catch (err) {
			this.rows = [
				{
					kind: 'error',
					id: `local-${Date.now()}`,
					text: formatError(err),
				},
			];
		}
	}

	/** Drop the in-memory session and start a blank one. The
	 *  panel renders the empty-session state until the user sends
	 *  the first prompt; that prompt creates the disk-backed file
	 *  via the `coder_send` path. */
	async newSession(): Promise<void> {
		try {
			await ipc.coder.newSession();
			this.rows = [];
			this.subagentSummaries = new Map();
			this.subagentTranscripts = new Map();
			this.viewSubagentId = null;
			this.activeSession = null;
			this.view = 'session';
			this.busy = false;
		} catch (err) {
			this.rows = [{ kind: 'error', id: `local-${Date.now()}`, text: formatError(err) }];
		}
	}

	/** Delete a persisted session (with no extra UI confirmation
	 *  here — callers wrap in a `confirm()` dialog). Idempotent
	 *  on the backend, so a double-click is safe. */
	async deleteSession(id: string): Promise<void> {
		try {
			await ipc.coder.deleteSession(id);
			await this.refreshSessions();
			if (this.activeSession?.id === id) {
				this.activeSession = null;
				this.rows = [];
				this.subagentSummaries = new Map();
				this.subagentTranscripts = new Map();
				this.viewSubagentId = null;
			}
		} catch (err) {
			// eslint-disable-next-line no-console
			console.warn('coder: failed to delete session', err);
		}
	}

	/** Switch to the sessions-list view. Doesn't drop the
	 *  in-memory session — the user can come back via a click. */
	showSessionsList(): void {
		this.view = 'list';
		void this.refreshSessions();
	}

	/** Switch to the transcript view of the current session. If
	 *  there's no in-memory session at all, this still flips the
	 *  view (the panel renders the "send your first message"
	 *  state). */
	showSessionView(): void {
		this.view = 'session';
	}

	closeModal(): void {
		this.deviceCode = null;
		this.awaitingApproval = false;
		this.startingFlow = false;
	}

	async refreshStatus(): Promise<void> {
		try {
			const next = await ipc.coder.status();
			this.status = next;
			// `next.busy` reflects the **active folder's** turn — the
			// backend filters by active folder so other folders'
			// background turns don't leak into this pip. The bucket
			// keeps a per-folder `busy` that flips on the live
			// event stream; this status probe just reconciles the
			// active bucket on mount / container-state transitions.
			this.busy = next.busy;
		} catch {
			// Status probe failures are silent: the panel still
			// renders the empty state and the next user action
			// (sign-in attempt, send) will surface the real error.
		}
	}

	/** Update the cached active-folder pointer. Called from
	 *  `state.svelte.ts` whenever the workspace's active folder
	 *  changes (initial bind, switch, removal). All per-folder
	 *  field accessors (`coder.rows`, `coder.busy`, …) re-resolve
	 *  through `current` after this update, so swapping projects
	 *  is "render the new bucket's `$state` against the panel" —
	 *  the previous folder's running turn keeps streaming events
	 *  into its own bucket in the background. */
	setActiveFolder(path: string | null): void {
		this.activeFolderPath = path;
	}

	async wireRuntime(): Promise<void> {
		if (this.#runtimeWired) {
			return;
		}
		this.#runtimeWired = true;
		try {
			const unlisten = await listen<CoderEventEnvelope>(CODER_EVENT_CHANNEL, (event) => {
				this.#dispatchEnvelope(event.payload);
			});
			this.#unlisten.push(unlisten);
		} catch {
			// Tauri event-bus bind failed. The panel is still
			// usable for sign-in; turns will appear stuck because
			// no events arrive. There's no actionable surface to
			// show the user, so we log to console only.
			// eslint-disable-next-line no-console
			console.warn('coder: failed to bind event channel');
		}
		// Re-probe status whenever the workspace shell container
		// changes state — the bash-target pip needs to flip the
		// moment the user clicks "Set up" / "Pause" / "Resume" or
		// the daemon transitions on its own. Same `container:state`
		// channel `container.svelte.ts` listens to.
		try {
			const unlisten = await listen('container:state', () => {
				void this.refreshStatus();
			});
			this.#unlisten.push(unlisten);
		} catch {
			// Same swallow as above — container events failing means
			// the pip just won't auto-update; the next status probe
			// (folder switch, manual reload) reconciles.
		}
		await this.refreshStatus();
		await this.#hydrateSession();
	}

	/** First-mount session hydration for the **active folder**.
	 *  Tries to restore the active folder's in-memory session
	 *  the backend already has (e.g. survived an HMR reload);
	 *  if there's none, falls back to the remembered
	 *  `last_session_by_folder[<active>]` from `AppState`;
	 *  if that's also missing or no longer exists, shows the
	 *  sessions list view. Best-effort throughout.
	 *
	 *  Per-folder per the multi-session design: each folder
	 *  hydrates independently. Switching folders doesn't
	 *  re-hydrate; folders the user has already visited keep
	 *  their bucket as it stands. */
	async #hydrateSession(): Promise<void> {
		try {
			const active = await ipc.coder.activeSession();
			if (active) {
				this.activeSession = active;
				this.view = 'session';
				return;
			}
		} catch {
			// fall through to last-session pointer
		}
		try {
			const appState = await ipc.appState.load();
			const folderKey = this.activeFolderPath ?? NO_FOLDER_KEY;
			const id = appState.coder.last_session_by_folder[folderKey];
			if (id) {
				try {
					await this.openSession(id);
					return;
				} catch {
					// Stale pointer — the session was deleted, or
					// we just switched to a folder that doesn't
					// have it. Drop into the list view.
				}
			}
		} catch {
			// AppState load failures are already toast-surfaced
			// from `state.svelte.ts:restoreAppState` on the main
			// path; no need to double-toast here.
		}
		await this.refreshSessions();
		this.view = (this.sessions?.length ?? 0) > 0 ? 'list' : 'session';
	}

	async startDeviceFlow(): Promise<void> {
		this.signInError = null;
		this.startingFlow = true;
		try {
			this.deviceCode = await ipc.coder.startDeviceFlow();
		} catch (err) {
			this.signInError = formatError(err);
			this.deviceCode = null;
			return;
		} finally {
			this.startingFlow = false;
		}
		const code = this.deviceCode;
		if (code === null) {
			return;
		}
		this.awaitingApproval = true;
		try {
			await ipc.coder.pollDeviceCode(code);
			await this.refreshStatus();
			this.deviceCode = null;
		} catch (err) {
			this.signInError = formatError(err);
		} finally {
			this.awaitingApproval = false;
		}
	}

	async signOut(): Promise<void> {
		try {
			await ipc.coder.signOut();
		} catch (err) {
			this.signInError = formatError(err);
			return;
		}
		this.rows = [];
		this.subagentSummaries = new Map();
		this.subagentTranscripts = new Map();
		this.viewSubagentId = null;
		this.busy = false;
		await this.refreshStatus();
	}

	async send(): Promise<void> {
		const text = this.draft.trim();
		const attachments = this.attachments;
		// Allow sending when *either* there's text or there are
		// attached selections — "explain this" with an attachment
		// but no question is a perfectly reasonable starter.
		if ((text.length === 0 && attachments.length === 0) || this.busy) {
			return;
		}
		const payload = renderPromptWithAttachments(text, attachments);
		this.draft = '';
		this.clearAttachments();
		// Optimistic flip — the `user_message` event lands within
		// milliseconds and reconciles, but the composer needs to
		// disable immediately or the user can fire a second turn
		// before the round-trip completes.
		this.busy = true;
		try {
			await ipc.coder.send(payload);
		} catch (err) {
			this.busy = false;
			this.rows = [
				...this.rows,
				{
					kind: 'error',
					id: `local-${Date.now()}`,
					text: formatError(err),
				},
			];
		}
	}

	async abort(): Promise<void> {
		try {
			await ipc.coder.abort();
		} catch {
			// Aborting a non-running turn is fine (idempotent on the
			// Rust side); we don't surface this.
		}
	}

	/** Top-level dispatch for an envelope arriving on
	 *  `coder:event`. Splits handling between:
	 *
	 *  - **Global events** — `folder_summary_ready` updates the
	 *    cross-folder description cache and is processed once,
	 *    regardless of which bucket the envelope arrived in.
	 *  - **Per-folder events** — everything else routes into the
	 *    bucket named by `envelope.folder`. Sub-agent events
	 *    naturally inherit their parent's folder (per the
	 *    backend's tagging contract), so a sub-agent's
	 *    transcript builds up in the same bucket as the parent
	 *    that spawned it. */
	#dispatchEnvelope(envelope: CoderEventEnvelope): void {
		if (envelope.event.kind === 'folder_summary_ready') {
			const next = new Map(this.folderDescriptions);
			next.set(envelope.event.folder, envelope.event.description);
			this.folderDescriptions = next;
			return;
		}
		const bucket = this.bucketFor(envelope.folder);
		this.#applyEventToBucket(bucket, envelope.folder, envelope.event);
	}

	/** Reduce one inner event into `bucket`. Mirrors the
	 *  pre-multi-session `#applyEvent` body, with `this.X` reads
	 *  replaced by `bucket.X`. The `folder` argument is needed
	 *  for `session_list_changed` (we may need to refresh a
	 *  non-active folder's session list). */
	#applyEventToBucket(bucket: FolderViewState, folder: string, event: CoderEvent): void {
		switch (event.kind) {
			case 'user_message':
				bucket.rows = [...bucket.rows, { kind: 'user', id: event.id, text: event.text }];
				bucket.busy = true;
				return;
			case 'assistant_message_start':
				// Insert the empty bubble so the user sees the row
				// land instantly, even before the model emits its
				// first token. Idempotent: the runner only fires
				// `start` once per id, but we'd no-op a duplicate
				// rather than insert a phantom row.
				if (bucket.rows.some((r) => r.kind === 'assistant' && r.id === event.id)) {
					return;
				}
				bucket.rows = [...bucket.rows, { kind: 'assistant', id: event.id, text: '', thinking: '', thinkingOpen: true }];
				return;
			case 'assistant_message_delta':
				bucket.rows = appendDelta(bucket.rows, event.id, event.delta, 'text');
				return;
			case 'assistant_thinking_delta':
				bucket.rows = appendDelta(bucket.rows, event.id, event.delta, 'thinking');
				return;
			case 'assistant_message_end':
				// Canonical replacement at close — see the file
				// header for the rationale (drift between
				// concatenated deltas and the final assembly heals
				// on close, plus markdown rendering re-runs once on
				// the complete text). Auto-collapse the thinking
				// block: the user already saw it stream, the answer
				// is the takeaway.
				bucket.rows = bucket.rows.map((row) =>
					row.kind === 'assistant' && row.id === event.id
						? { ...row, text: event.text, thinking: event.thinking ?? row.thinking, thinkingOpen: false }
						: row,
				);
				return;
			case 'tool_call':
				bucket.rows = [
					...bucket.rows,
					{
						kind: 'tool',
						id: event.id,
						name: event.name,
						args: event.args,
						result: undefined,
						hasResult: false,
						isError: false,
						startedAt: Date.now(),
						durationMs: null,
					},
				];
				return;
			case 'tool_result':
				bucket.rows = bucket.rows.map((row) =>
					row.kind === 'tool' && row.id === event.id
						? {
								...row,
								result: event.result,
								hasResult: true,
								isError: event.is_error,
								durationMs: Date.now() - row.startedAt,
							}
						: row,
				);
				return;
			case 'turn_complete':
				bucket.busy = false;
				return;
			case 'aborted':
				bucket.busy = false;
				bucket.rows = [...bucket.rows, { kind: 'aborted', id: `aborted-${Date.now()}` }];
				return;
			case 'error':
				bucket.busy = false;
				bucket.rows = [
					...bucket.rows,
					{
						kind: 'error',
						id: `error-${Date.now()}`,
						text: event.message,
					},
				];
				return;
			case 'session_loaded':
				// Reset the bucket's transcript and adopt the new
				// session's metadata. Replay events arrive
				// immediately after this one (fired by the backend
				// on the same `coder:event` channel), so the rows
				// fill in on the next handlers.
				bucket.rows = [];
				bucket.subagentSummaries = new Map();
				bucket.subagentTranscripts = new Map();
				bucket.viewSubagentId = null;
				bucket.busy = false;
				bucket.activeSession = {
					id: event.id,
					title: event.title,
					created_at_ms: event.created_at_ms,
					updated_at_ms: event.updated_at_ms,
				};
				bucket.view = 'session';
				return;
			case 'session_title_updated':
				if (bucket.activeSession?.id === event.id) {
					bucket.activeSession = { ...bucket.activeSession, title: event.title };
				}
				if (bucket.sessions !== null) {
					bucket.sessions = bucket.sessions.map((s) => (s.id === event.id ? { ...s, title: event.title } : s));
				}
				return;
			case 'session_list_changed':
				// Re-fetch the folder's session list. We can only
				// re-fetch the **active** folder's list via the
				// existing `coder_list_sessions` API (it uses the
				// active folder server-side). For non-active
				// folders, the bucket's `sessions` cache will go
				// stale until the user switches back; cheap to
				// live with — the next visit refreshes via
				// `refreshSessions`.
				if (folder === (this.activeFolderPath ?? NO_FOLDER_KEY)) {
					void this.refreshSessions();
				} else {
					// Mark stale so the next visit force-refetches.
					bucket.sessions = null;
				}
				return;
			case 'folder_summary_ready':
				// Handled at the envelope level — see
				// `#dispatchEnvelope`. Should never reach this
				// arm; keep the case for exhaustiveness.
				return;
			case 'subagent_spawned': {
				const summary: SubagentSummary = {
					id: event.subagent_id,
					toolCallId: event.tool_call_id,
					targetFolder: event.target_folder,
					mode: event.mode,
					status: 'running',
					resultPreview: null,
					tokensUsedEstimate: 0,
					subSessionId: null,
				};
				const summaries = new Map(bucket.subagentSummaries);
				summaries.set(event.tool_call_id, summary);
				bucket.subagentSummaries = summaries;

				const transcripts = new Map(bucket.subagentTranscripts);
				transcripts.set(event.subagent_id, {
					id: event.subagent_id,
					toolCallId: event.tool_call_id,
					mode: event.mode,
					targetFolder: event.target_folder,
					rows: [],
				});
				bucket.subagentTranscripts = transcripts;
				return;
			}
			case 'subagent_event': {
				const transcripts = new Map(bucket.subagentTranscripts);
				const existing = transcripts.get(event.subagent_id);
				if (!existing) {
					return;
				}
				const nextRows = applyInnerEventToRows(existing.rows, event.inner);
				transcripts.set(event.subagent_id, { ...existing, rows: nextRows });
				bucket.subagentTranscripts = transcripts;
				return;
			}
			case 'subagent_finished': {
				const summaries = new Map(bucket.subagentSummaries);
				const summary = findSummaryById(summaries, event.subagent_id);
				if (!summary) {
					return;
				}
				const transcript = bucket.subagentTranscripts.get(event.subagent_id);
				const lastAssistant = transcript?.rows
					.toReversed()
					.find((row): row is Extract<CoderRow, { kind: 'assistant' }> => row.kind === 'assistant');
				const preview = lastAssistant?.text.trim() ?? null;
				summaries.set(summary.toolCallId, {
					...summary,
					status: event.was_error ? 'error' : 'done',
					resultPreview: preview && preview.length > 0 ? preview : summary.resultPreview,
					tokensUsedEstimate: event.tokens_used_estimate,
					subSessionId: summary.subSessionId,
				});
				bucket.subagentSummaries = summaries;
				return;
			}
			case 'token_usage':
				bucket.tokenUsage = {
					prompt: event.prompt_tokens,
					completion: event.completion_tokens,
					total: event.total_tokens,
					contextWindow: event.context_window,
					source: event.source,
				};
				return;
			case 'compaction_started':
				bucket.compaction = {
					phase: 'running',
					messagesCompacted: event.messages_compacted,
				};
				return;
			case 'compaction_complete': {
				const previous = bucket.compaction;
				bucket.compaction = {
					phase: 'done',
					messagesCompacted: previous?.phase === 'running' ? previous.messagesCompacted : 0,
					summary: event.summary,
					promptTokensAfter: event.prompt_tokens_after,
				};
				// Mirror the backend's "reset trigger after compaction
				// runs" so the ring shows the new (lower) prompt size
				// immediately rather than waiting for the next
				// `token_usage` event to land.
				if (bucket.tokenUsage) {
					bucket.tokenUsage = {
						...bucket.tokenUsage,
						prompt: event.prompt_tokens_after,
					};
				}
				return;
			}
		}
	}

	/** Switch the panel into the sub-agent pop-out view. The
	 *  back-arrow in `'subagent'` mode returns to the parent's
	 *  session via [`closeSubagentView`]. */
	openSubagent(subagentId: string): void {
		if (!this.subagentTranscripts.has(subagentId)) {
			return;
		}
		this.viewSubagentId = subagentId;
		this.view = 'subagent';
	}

	/** Return from a sub-agent pop-out to the parent's session
	 *  transcript. Keeps the sub-agent's state in
	 *  `subagentTranscripts` so re-opening the same card lands at
	 *  the same place. */
	closeSubagentView(): void {
		this.viewSubagentId = null;
		this.view = 'session';
	}
}

/** Find a `SubagentSummary` in `summaries` whose `id` matches.
 *  Used by `subagent_finished` (which carries `subagent_id`, not
 *  the parent's `tool_call_id` we keyed by). */
function findSummaryById(summaries: Map<string, SubagentSummary>, subagentId: string): SubagentSummary | null {
	for (const summary of summaries.values()) {
		if (summary.id === subagentId) {
			return summary;
		}
	}
	return null;
}

/** Reduce one inner sub-agent event onto a row list. Mirrors the
 *  parent's `#applyEvent` row mutations for the assistant + tool
 *  cases — the only event kinds a sub-agent ever wraps. Other
 *  kinds (turn_complete, session_*, error, aborted) belong to the
 *  parent's lifecycle and are handled at the outer level. */
function applyInnerEventToRows(rows: CoderRow[], event: CoderEvent): CoderRow[] {
	switch (event.kind) {
		case 'assistant_message_start':
			return [...rows, { kind: 'assistant', id: event.id, text: '', thinking: '', thinkingOpen: true }];
		case 'assistant_message_delta':
			return appendDelta(rows, event.id, event.delta, 'text');
		case 'assistant_thinking_delta':
			return appendDelta(rows, event.id, event.delta, 'thinking');
		case 'assistant_message_end':
			return rows.map((row) =>
				row.kind === 'assistant' && row.id === event.id
					? { ...row, text: event.text, thinking: event.thinking ?? row.thinking, thinkingOpen: false }
					: row,
			);
		case 'tool_call':
			return [
				...rows,
				{
					kind: 'tool',
					id: event.id,
					name: event.name,
					args: event.args,
					result: undefined,
					hasResult: false,
					isError: false,
					startedAt: Date.now(),
					durationMs: null,
				},
			];
		case 'tool_result':
			return rows.map((row) =>
				row.kind === 'tool' && row.id === event.id
					? {
							...row,
							result: event.result,
							hasResult: true,
							isError: event.is_error,
							durationMs: Date.now() - row.startedAt,
						}
					: row,
			);
		default:
			return rows;
	}
}

/** Build the user-message string we ship to the model. Mirrors
 *  Cursor's wire shape: the user's prose stays intact (with the
 *  `@path:start-end` tokens inline at the spots the user picked
 *  via Ctrl+L), and the resolved snippet contents land in a
 *  trailing `<context>` block of `<code_selection>` elements.
 *  Splitting the two means a multi-attachment prompt reads
 *  naturally ("compare `@a.rs:10-20` and `@b.rs:5-15`") instead
 *  of inflating the prose with a wall of code headers.
 *
 *  Empty draft + non-empty attachments is a valid send — we ship
 *  just the context block so "explain this" with one selection
 *  works. */
function renderPromptWithAttachments(text: string, attachments: ComposerAttachment[]): string {
	if (attachments.length === 0) {
		return text;
	}
	const blocks = attachments.map((att) => {
		const range = att.startLine === att.endLine ? `${att.startLine}` : `${att.startLine}-${att.endLine}`;
		// Wrap the captured text verbatim. We don't fence the body
		// since the surrounding `<code_selection>` element is
		// already an unambiguous delimiter — no risk of
		// triple-backticks in the snippet "closing" our wrapper.
		return `<code_selection path="${escapeXmlAttr(att.path)}" lines="${range}">\n${att.text}\n</code_selection>`;
	});
	const context = `<context>\n${blocks.join('\n')}\n</context>`;
	return text.length > 0 ? `${text}\n\n${context}` : context;
}

const REGEX_META_PATTERN = /[\\^$.*+?()[\]{}|]/g;

function escapeRegExp(s: string): string {
	return s.replace(REGEX_META_PATTERN, '\\$&');
}

function escapeXmlAttr(s: string): string {
	return s.replace(/&/g, '&amp;').replace(/"/g, '&quot;').replace(/</g, '&lt;');
}

/** Stable shape for the inline `@`-token. The model sees the
 *  same form Cursor emits, which means no prompt-engineering
 *  retraining for the surface it's already learned. */
function formatAttachmentToken(path: string, startLine: number, endLine: number): string {
	if (startLine === endLine) {
		return `@${path}:${startLine}`;
	}
	return `@${path}:${startLine}-${endLine}`;
}

/** Append `delta` to the assistant row identified by `id`. The
 *  `field` selector picks which sub-string accumulates: `'text'`
 *  for the visible answer, `'thinking'` for the reasoning trace.
 *  If no row with that id exists yet (a delta arrived before the
 *  matching `assistant_message_start` — defensive against future
 *  provider quirks), insert a fresh row carrying just the delta. */
function appendDelta(rows: CoderRow[], id: string, delta: string, field: 'text' | 'thinking'): CoderRow[] {
	let found = false;
	const next = rows.map((row) => {
		if (row.kind === 'assistant' && row.id === id) {
			found = true;
			return { ...row, [field]: row[field] + delta };
		}
		return row;
	});
	if (!found) {
		const seed: CoderRow = {
			kind: 'assistant',
			id,
			text: field === 'text' ? delta : '',
			thinking: field === 'thinking' ? delta : '',
			thinkingOpen: true,
		};
		next.push(seed);
	}
	return next;
}

export const coder = new CoderPanelState();
