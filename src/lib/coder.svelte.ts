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
	type CoderHubBucket,
	type CoderModelSettings,
	type CoderProviderConfig,
	type CoderSessionSummary,
	type CoderStatus,
	type DeviceCode,
	type HfIdentity,
	type HubNamespace,
	type ImageAttachmentPayload,
	type ProviderKind,
	type ProviderModelSummary,
	type ProviderProbeResult,
	type RouterModel,
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
	| {
			kind: 'user';
			id: string;
			text: string;
			images: ImageAttachmentPayload[];
			/** `true` while the message is sitting in the runner's
			 *  pending-steers queue (sent during an ongoing turn
			 *  and not yet drained into the chat). The panel
			 *  renders these rows in a muted "queued" style and
			 *  the composer's `Ctrl+Up` un-queue gesture only
			 *  targets queued rows. Flips to `false` on the
			 *  matching `steer_drained` event. */
			queued: boolean;
	  }
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
 *  transcript. Drives the collapsed card under each `task`
 *  tool row: `running` while events stream in, `done` /
 *  `error` / `aborted` once `subagent_finished` lands. */
export type SubagentStatus = 'running' | 'done' | 'error';

/** Summary card displayed inline under a `task` tool
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

/** One editor selection the user attached to the composer via
 *  the Ctrl+L "add to chat" gesture (mirrors Cursor's
 *  `@file:line-line` chips). The text is captured at attach time
 *  so a follow-up edit to the file doesn't change what the agent
 *  sees — the user pinned a snapshot, not a reference.
 *
 *  Each attachment has a stable `token` (`@path:start-end`) that
 *  also lives inline in the composer textarea — same shape
 *  Cursor uses. Send-time formatting reads this token to decide
 *  the order of attachments in the trailing `<context>` block,
 *  and the panel's `×` button strips matching tokens out of the
 *  draft so the chip and the inline reference always agree. */
export type SelectionAttachment = {
	kind: 'selection';
	id: string;
	token: string;
	path: string;
	startLine: number;
	endLine: number;
	text: string;
};

/** One image the user pasted (or otherwise dropped) into the
 *  composer. Stored as a `data:<mime>;base64,...` URL — the same
 *  shape providers want on the wire — so the send path doesn't
 *  have to re-encode at the last second. `name` is purely
 *  cosmetic (chip label / accessibility), `sizeBytes` drives the
 *  pre-send size cap so a 10 MB screenshot doesn't quietly blow
 *  the provider's request limit. */
export type ImageComposerAttachment = {
	kind: 'image';
	id: string;
	dataUrl: string;
	mime: string;
	name: string;
	sizeBytes: number;
};

/** A snapshot of text the user selected in a terminal pane and
 *  attached via Ctrl+L. `label` is the source terminal's tab title
 *  (typically the cwd basename) for chip readability when several
 *  terminals are open. `lineCount` is captured at attach time for
 *  the chip label so the user can see how much they grabbed
 *  without expanding the chip. `token` is the inline reference we
 *  splice into the draft (e.g. `@terminal:powergrid` —
 *  disambiguated with a `#N` suffix when multiple captures share
 *  the same label) so the model has an in-prose pointer to the
 *  matching `<terminal_output>` element in the trailing
 *  `<context>` block. */
export type TerminalAttachment = {
	kind: 'terminal';
	id: string;
	token: string;
	text: string;
	label: string;
	lineCount: number;
};

/** Anything the chip strip can hold. The three shapes share the
 *  panel's render path (one chip per attachment) but differ in
 *  what the user clicks them for: selections jump to the file at
 *  the captured range; images preview the picture; terminal
 *  attachments are read-only context blobs (the scrollback isn't
 *  navigable). Send-time splits them — selection / terminal
 *  render into the trailing `<context>` block, images ride on
 *  the IPC alongside `text`. */
export type ComposerAttachment = SelectionAttachment | ImageComposerAttachment | TerminalAttachment;

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
/**
 * One entry in the agent's session-scoped todo list. Mirrors
 * `moon_coder::TodoItem`. The pill in the panel header and the
 * `ToolBodyTodoWrite.svelte` row body both render off these.
 */
export type TodoItem = {
	id: string;
	content: string;
	status: 'pending' | 'in_progress' | 'completed' | 'cancelled';
};

/** Set keyed by `string` (not `TodoItem['status']`) so calling
 *  `.has(unknown)` after a `typeof === 'string'` guard doesn't
 *  need an unsafe narrowing cast. */
const TODO_STATUSES: ReadonlySet<string> = new Set(['pending', 'in_progress', 'completed', 'cancelled']);

function isTodoStatus(value: unknown): value is TodoItem['status'] {
	return typeof value === 'string' && TODO_STATUSES.has(value);
}

/**
 * Pull the canonical todo list out of a `todo_write` tool result
 * payload. Returns `null` when the shape doesn't match (older
 * traces, error payloads, future shape drift) so the caller can
 * leave the bucket's list untouched.
 */
function extractTodos(result: unknown): TodoItem[] | null {
	if (typeof result !== 'object' || result === null) {
		return null;
	}
	const raw = (result as { todos?: unknown }).todos;
	if (!Array.isArray(raw)) {
		return null;
	}
	// `Array.isArray` widens `raw` to `any[]` in TS's flow
	// analysis; re-assert to `unknown[]` so the per-item cast
	// below narrows from `unknown` (oxlint allows that) rather
	// than from `any` (oxlint flags as unsafe).
	const items: unknown[] = raw;
	const out: TodoItem[] = [];
	for (const item of items) {
		if (typeof item !== 'object' || item === null) {
			return null;
		}
		const o = item as { id?: unknown; content?: unknown; status?: unknown };
		if (typeof o.id !== 'string' || typeof o.content !== 'string') {
			return null;
		}
		if (!isTodoStatus(o.status)) {
			return null;
		}
		out.push({ id: o.id, content: o.content, status: o.status });
	}
	return out;
}

class FolderViewState {
	rows = $state<CoderRow[]>([]);
	busy = $state(false);
	/** "An agent in this folder finished a turn while the user
	 *  wasn't looking, and they haven't visited the folder
	 *  since." Drives the static amber sparkle on the folder bar
	 *  for non-active projects with completed work, so a user
	 *  juggling background agents notices "that one's done"
	 *  without needing the panel open. Set on
	 *  `turn_complete` / `aborted` / `error` only for buckets
	 *  whose folder is not currently active (an active-folder
	 *  completion is something the user is already looking at).
	 *  Cleared in [`setActiveFolder`] when the user switches to
	 *  the folder. Process-local; no need to persist across
	 *  restarts. */
	attentionPending = $state(false);
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
	/** Canonical post-merge todo list maintained by the agent's
	 *  `todo_write` tool. Mirrored from `tool_result.todos` so the
	 *  pill / popover in the panel header stay in lock-step with
	 *  the model's view. Empty until the agent calls the tool;
	 *  also re-seeded on session replay because `tool_result`
	 *  events are re-emitted as part of the replay stream. */
	todos = $state<TodoItem[]>([]);
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
	/** Anthropic prompt-caching breakdown of `prompt`, surfaced by
	 *  OpenRouter when the request carried `cache_control: ephemeral`
	 *  markers (currently set automatically when the active provider
	 *  is OpenRouter and the model id is `anthropic/...`; see
	 *  `cache_breakpoint_indexes` in `crates/moon-coder/src/inference.rs`).
	 *
	 *  - `cacheReadTokens`: how many of `prompt` were served from
	 *    the 5-min ephemeral cache at the 90 % discount. The bigger
	 *    this number, the more the call saved off the base input
	 *    price.
	 *  - `cacheCreationTokens`: how many of `prompt` were written
	 *    to cache on this call at the 25 % surcharge. Pays back on
	 *    the very next call within 5 min, as long as the prefix
	 *    stays stable.
	 *
	 *  Both are `0` for non-Anthropic providers / requests with no
	 *  cache markers. They are a subset of `prompt`, not a delta:
	 *  `prompt` is the full input count regardless of how it was
	 *  billed. */
	cacheReadTokens: number;
	cacheCreationTokens: number;
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

/** Per-session sync state for the row decoration in the session
 *  list. Held in `CoderPanelState.hubSyncState`, keyed by
 *  session id. Driven by the streamed `HubSyncStarted` /
 *  `HubSyncFinished` events — the row icon flips based on this
 *  state, not the IPC return value (which only signals "request
 *  accepted"). */
export type HubSyncRowState =
	| { phase: 'syncing' }
	| { phase: 'synced'; atMs: number }
	| { phase: 'failed'; error: string };

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

	/** Snapshot of the user's current model picks, mirrored from
	 *  `coder_get_model_settings`. `null` until the popover (or any
	 *  other consumer) calls `loadModelSettings()` for the first
	 *  time. Writes go through `saveModelSettings()` which both
	 *  persists and re-reads, so we never have to optimistically
	 *  update this from the UI. */
	modelSettings = $state<CoderModelSettings | null>(null);

	/** Router `/v1/models` catalog. Populated by
	 *  `loadModels()` on popover-open. `null` until the first fetch;
	 *  callers can use the loading flag to decide whether to show a
	 *  spinner. Lives at the panel level (not popover-local) so a
	 *  reopen inside the same session reuses the cached list
	 *  without re-hitting the network. */
	routerModels = $state<RouterModel[] | null>(null);

	/** UI flag while `coder_list_models` is in flight. */
	modelsLoading = $state(false);

	/** Last error from `coder_list_models` / `coder_set_model_settings`,
	 *  surfaced inline in the popover so the user can see what the
	 *  router said. Cleared on the next successful call. */
	modelsError = $state<string | null>(null);

	/** HF Hub bucket bound to the current workspace, if any.
	 *  Populated by [`loadHubBinding`] on panel mount and after a
	 *  successful create/disconnect. `null` means "no binding";
	 *  the picker renders the "Connect" affordance in that case. */
	hubBucket = $state<CoderHubBucket | null>(null);

	/** Per-session sync state, keyed by session id. Lives on the
	 *  panel state so the row decoration can flip between
	 *  "syncing…" / "synced" / "failed" without dragging the full
	 *  binding through props. Populated by `HubSyncStarted` /
	 *  `HubSyncFinished` envelopes. */
	hubSyncState = $state<Record<string, HubSyncRowState>>({});

	/** Fetch the current settings into [`modelSettings`]. Safe to
	 *  call repeatedly; idempotent at the steady state. Errors land
	 *  in [`modelsError`] and the previous snapshot stays in place
	 *  so a stale popover still has something to render. */
	async loadModelSettings(): Promise<void> {
		try {
			this.modelSettings = await ipc.coder.getModelSettings();
		} catch (err) {
			this.modelsError = formatError(err);
		}
	}

	/** Persist + apply the new settings and refresh the snapshot.
	 *  Throws so the popover can keep the form open on failure;
	 *  caller decides what to render in that state. */
	async saveModelSettings(next: CoderModelSettings): Promise<void> {
		try {
			await ipc.coder.setModelSettings(next);
			this.modelSettings = next;
			this.modelsError = null;
		} catch (err) {
			this.modelsError = formatError(err);
			throw err;
		}
	}

	/** Fetch the current workspace's HF Hub binding into
	 *  [`hubBucket`]. Safe to call repeatedly; idempotent. Errors
	 *  drop silently — the picker just keeps the "Connect"
	 *  affordance visible until the next reload. The picker
	 *  surfaces real-time failures via the modal / row tooltips
	 *  rather than a panel-level toast, so we don't push the
	 *  read failure anywhere globally either. */
	async loadHubBinding(): Promise<void> {
		try {
			this.hubBucket = await ipc.coder.hubGetBinding();
		} catch {
			// Ignored — see fn docstring.
		}
	}

	/** List the HF namespaces the user can create a bucket under
	 *  (their login + every org they belong to). Used by the
	 *  connect modal's dropdown. Throws so the modal can surface
	 *  network / not-signed-in failures inline. */
	async listHubNamespaces(): Promise<HubNamespace[]> {
		return await ipc.coder.hubListNamespaces();
	}

	/** Provision a bucket on the Hub and bind it to the active
	 *  workspace. Updates [`hubBucket`] on success. The modal
	 *  reads the return value to render the post-create banner;
	 *  it's also stored on [`hubBucket`] so re-opening the
	 *  picker sees the connected state. */
	async createHubBucket(namespace: string, name: string, isPrivate: boolean): Promise<CoderHubBucket> {
		const bucket = await ipc.coder.hubCreateBucket(namespace, name, isPrivate);
		this.hubBucket = bucket;
		return bucket;
	}

	/** Flip autosync on or off. Optimistic update; reloads the
	 *  binding on failure to recover from a stale flag. */
	async setHubAutosync(enabled: boolean): Promise<void> {
		const previous = this.hubBucket;
		if (this.hubBucket) {
			this.hubBucket = { ...this.hubBucket, autosync: enabled };
		}
		try {
			await ipc.coder.hubSetAutosync(enabled);
		} catch (err) {
			this.hubBucket = previous;
			throw err;
		}
	}

	/** Drop the workspace's binding. Does not touch the bucket on
	 *  the Hub itself — that's a web-UI action. */
	async disconnectHubBucket(): Promise<void> {
		await ipc.coder.hubDisconnect();
		this.hubBucket = null;
		this.hubSyncState = {};
	}

	/** Push one session JSONL to the Hub right now. Used by the
	 *  per-row upload icon and the header "Sync all" button.
	 *  Returns the same promise the IPC resolves so callers can
	 *  await for chained UI updates (the row state itself flips
	 *  via the streamed `HubSyncStarted` / `HubSyncFinished`
	 *  events, not from this resolution). */
	async uploadSessionToHub(sessionId: string): Promise<void> {
		await ipc.coder.hubUploadSession(sessionId);
	}

	/** Fetch the router catalog. One round trip per call. The result
	 *  is cached in [`routerModels`]; consumers should check that
	 *  first and only call this when it's `null` (or when forcing a
	 *  refresh on an explicit user gesture). HF-only — call
	 *  [`loadProviderModels`] when a user provider is active. */
	async loadModels(): Promise<void> {
		if (this.modelsLoading) {
			return;
		}
		this.modelsLoading = true;
		try {
			this.routerModels = await ipc.coder.listModels();
			this.modelsError = null;
		} catch (err) {
			this.modelsError = formatError(err);
		} finally {
			this.modelsLoading = false;
		}
	}

	/** Flat `/v1/models` catalog for a user-added provider.
	 *  Cached per-id in [`providerModels`] so flipping back to
	 *  the same provider doesn't re-hit the network. The
	 *  picker calls this every time the active provider
	 *  changes; an explicit `Refresh` action flushes the cache. */
	providerModels = $state<Record<string, ProviderModelSummary[] | null>>({});

	async loadProviderModels(id: string): Promise<void> {
		if (this.modelsLoading) {
			return;
		}
		this.modelsLoading = true;
		try {
			const rows = await ipc.coder.listProviderModels(id);
			this.providerModels = { ...this.providerModels, [id]: rows };
			this.modelsError = null;
		} catch (err) {
			// Cache `[]` so the picker stops spinning + renders the
			// "type a model id directly" hint. The error itself
			// stays in `modelsError` for the inline message.
			this.providerModels = { ...this.providerModels, [id]: [] };
			this.modelsError = formatError(err);
		} finally {
			this.modelsLoading = false;
		}
	}

	/** Forget the cached `/v1/models` rows for `id`. Used by the
	 *  Edit-provider flow after a `base_url` change so the
	 *  picker re-fetches against the new URL. */
	forgetProviderModels(id: string): void {
		const next = { ...this.providerModels };
		delete next[id];
		this.providerModels = next;
	}

	/** Probe a `(base_url, api_key)` pair from the Add/Edit
	 *  provider modal. Throws the formatted error so the modal
	 *  keeps the form open on failure. */
	async probeProvider(baseUrl: string, apiKey: string, kind: ProviderKind = 'custom'): Promise<ProviderProbeResult> {
		try {
			return await ipc.coder.probeProvider(baseUrl, apiKey, kind);
		} catch (err) {
			throw new Error(formatError(err), { cause: err });
		}
	}

	/** Add or update a provider entry. The picker calls
	 *  `setProviderApiKey` separately for the API key (it's
	 *  keyring-bound and never round-trips through this shape).
	 *  Refreshes [`modelSettings`] from the runner so the new
	 *  shape lands without a manual round-trip. */
	async saveProvider(config: CoderProviderConfig): Promise<void> {
		try {
			await ipc.coder.saveProvider(config);
			await this.loadModelSettings();
			this.modelsError = null;
		} catch (err) {
			this.modelsError = formatError(err);
			throw err;
		}
	}

	/** Delete a provider entry + its keyring slot. */
	async deleteProvider(id: string): Promise<void> {
		try {
			await ipc.coder.deleteProvider(id);
			this.forgetProviderModels(id);
			await this.loadModelSettings();
			this.modelsError = null;
		} catch (err) {
			this.modelsError = formatError(err);
			throw err;
		}
	}

	/** Persist a per-provider API key. Empty values rejected by
	 *  the runner. Refreshes [`modelSettings`] so `has_api_key`
	 *  flips on the picker without manual coordination. */
	async setProviderApiKey(id: string, key: string): Promise<void> {
		try {
			await ipc.coder.setProviderApiKey(id, key);
			await this.loadModelSettings();
			this.modelsError = null;
		} catch (err) {
			this.modelsError = formatError(err);
			throw err;
		}
	}

	/** Drop a per-provider key. Idempotent. */
	async clearProviderApiKey(id: string): Promise<void> {
		try {
			await ipc.coder.clearProviderApiKey(id);
			await this.loadModelSettings();
			this.modelsError = null;
		} catch (err) {
			this.modelsError = formatError(err);
			throw err;
		}
	}

	/** Allocate a fresh opaque provider id from the runner. The
	 *  Add-provider modal uses this so the keyring slot for the
	 *  API key is addressable before the provider config lands
	 *  in `AppState`. */
	async newProviderId(): Promise<string> {
		return await ipc.coder.newProviderId();
	}

	/** True iff a Tavily API key is stored in the keyring. Tracked
	 *  so the model-settings popover can render the right state
	 *  (set / configured / clearing) without a keyring round-trip
	 *  every keystroke. `null` until the popover (or another
	 *  consumer) calls [`loadWebSearchConfigured`] for the first
	 *  time — distinguishes "we don't know yet" from "we know:
	 *  not configured". */
	webSearchConfigured = $state<boolean | null>(null);

	/** Refresh [`webSearchConfigured`] from the runner. Safe to
	 *  call repeatedly. */
	async loadWebSearchConfigured(): Promise<void> {
		try {
			this.webSearchConfigured = await ipc.coder.webSearchConfigured();
		} catch (err) {
			this.modelsError = formatError(err);
		}
	}

	/** Persist a new Tavily API key. Throws on validation failure
	 *  (empty key) so the popover can keep the form open + show the
	 *  message inline. */
	async saveWebSearchKey(key: string): Promise<void> {
		try {
			await ipc.coder.setWebSearchKey(key);
			this.webSearchConfigured = true;
			this.modelsError = null;
		} catch (err) {
			this.modelsError = formatError(err);
			throw err;
		}
	}

	/** Drop the Tavily key. Idempotent. */
	async clearWebSearchKey(): Promise<void> {
		try {
			await ipc.coder.clearWebSearchKey();
			this.webSearchConfigured = false;
			this.modelsError = null;
		} catch (err) {
			this.modelsError = formatError(err);
			throw err;
		}
	}

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

	/** "Has an agent in this folder finished a turn that the user
	 *  hasn't seen yet?" Same per-folder shape as [`busyForFolder`].
	 *  The folder bar reads through this so a switch back to the
	 *  folder (which clears the flag in [`setActiveFolder`]) re-
	 *  renders the bar without the badge. */
	attentionPendingForFolder(folder: string): boolean {
		return this.bucketFor(folder).attentionPending;
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

	/** Per-folder todo list. The header pill reads it directly via
	 *  `coder.todos`; the popover renders the same list with status
	 *  glyphs. Empty array when the agent hasn't called
	 *  `todo_write` in the current session. */
	get todos(): TodoItem[] {
		return this.current.todos;
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
	#listenersWired = false;
	/** Flipped to `true` by `markWorkspaceReady()` once the
	 *  `restoreAppState` folder-restore loop has finished mutating
	 *  the **backend's** active folder. Per-folder hydration
	 *  (`refreshStatus` + `#hydrateSession`) is gated on this so it
	 *  doesn't fire while the loop is racing the backend's
	 *  active-folder pointer through every persisted folder — that
	 *  race is what previously made the panel show another folder's
	 *  sessions on cold start (the panel's `coder.activeFolderPath`
	 *  was correct, but `coder_list_sessions` reads from the
	 *  backend's mutable active-folder pointer). */
	#workspaceReady = false;
	/** Folders we've already kicked off hydration for. Per-folder
	 *  so a switch between unvisited folders fetches fresh state;
	 *  switches back to a folder we already hydrated reuse the
	 *  bucket as it stands (per the multi-session "agents keep
	 *  running per project" contract). */
	#hydratedFolders = new Set<string>();

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
			(a) =>
				a.kind === 'selection' &&
				a.path === snapshot.path &&
				a.startLine === snapshot.startLine &&
				a.endLine === snapshot.endLine,
		);
		if (!dup) {
			this.attachments = [
				...this.attachments,
				{
					kind: 'selection',
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

	/** Open the panel + attach a terminal scrollback selection to
	 *  the composer. Bound to Ctrl+L when a terminal pane has the
	 *  active text selection. Dedupes on identical (text, label)
	 *  pairs so a stray double-tap doesn't pile two chips for the
	 *  exact same blob, and splices an `@terminal:<label>` token at
	 *  the caret so the model can refer back to the corresponding
	 *  `<terminal_output>` element in `<context>` from inside the
	 *  user's prose (mirrors the inline-token behaviour code
	 *  selections get). When several captures share the same label
	 *  we suffix `#2`, `#3`, … so the tokens stay distinct;
	 *  reattaching the *exact* same scrollback reuses the original
	 *  token so the draft already has the right pointer. */
	addAttachmentFromTerminal(snapshot: { text: string; label: string }): void {
		const text = snapshot.text;
		if (text.length === 0) {
			return;
		}
		rightPanel.set('coder');
		this.view = 'session';
		const dup = this.attachments.find(
			(a): a is TerminalAttachment => a.kind === 'terminal' && a.text === text && a.label === snapshot.label,
		);
		const token = dup ? dup.token : this.#nextTerminalToken(snapshot.label);
		if (!dup) {
			const lineCount = countLines(text);
			this.attachments = [
				...this.attachments,
				{
					kind: 'terminal',
					id: `att-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`,
					token,
					text,
					label: snapshot.label,
					lineCount,
				},
			];
		}
		this.#insertTokenAtCaret(token);
		this.composerFocusTick = this.composerFocusTick + 1;
	}

	/** Mint the next `@terminal:<label>` token that doesn't collide
	 *  with an existing terminal chip. `label` gets the same
	 *  whitespace-collapse + non-word-strip the chip already does,
	 *  so the token stays single-word (a stray space in the prose
	 *  would break the token-as-word convention shared with
	 *  selection tokens). */
	#nextTerminalToken(label: string): string {
		const base = `@terminal:${sanitiseTokenLabel(label)}`;
		const existing = new Set(
			this.attachments.filter((a): a is TerminalAttachment => a.kind === 'terminal').map((a) => a.token),
		);
		if (!existing.has(base)) {
			return base;
		}
		for (let n = 2; n < 1000; n++) {
			const candidate = `${base}#${n}`;
			if (!existing.has(candidate)) {
				return candidate;
			}
		}
		// 1000 simultaneous terminal chips means the user is doing
		// something very different from "drop a few logs in the
		// prompt", and any token we pick will work — pick a random
		// one and move on.
		return `${base}#${Date.now().toString(36)}`;
	}

	/** "Fix in coder" entry-point from the editor's lint tooltip.
	 *  Opens the panel, attaches a snapshot of the diagnostic's
	 *  range (so the model sees the same code the squiggle covers),
	 *  and seeds the composer draft with a one-line ask that
	 *  mentions the rule + first line of the linter's message.
	 *
	 *  The prompt is intentionally short — long pre-canned text
	 *  trains the user to delete it before sending, which is worse
	 *  than a tight starter line they can edit. The diagnostic's
	 *  full message and surrounding source are already attached as
	 *  the selection snippet, so the model isn't reading the
	 *  squiggle blind. */
	fixDiagnosticInCoder(args: {
		path: string;
		startLine: number;
		endLine: number;
		text: string;
		code: string | null;
		source: string | null;
		message: string;
	}): void {
		rightPanel.set('coder');
		this.view = 'session';
		const token = formatAttachmentToken(args.path, args.startLine, args.endLine);
		const dup = this.attachments.find(
			(a) =>
				a.kind === 'selection' && a.path === args.path && a.startLine === args.startLine && a.endLine === args.endLine,
		);
		if (!dup) {
			this.attachments = [
				...this.attachments,
				{
					kind: 'selection',
					id: `att-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`,
					token,
					path: args.path,
					startLine: args.startLine,
					endLine: args.endLine,
					text: args.text,
				},
			];
		}
		// Tag is whichever of source / code we know — `oxlint`
		// emits both (`source: "oxc"`, `code: "no-base-to-string"`),
		// `tsgo` emits just `code` (the TS error number). The
		// space between them is conditional so we never show a
		// stray leading separator when only one is set.
		const tagParts = [args.source, args.code].filter((s): s is string => typeof s === 'string' && s.length > 0);
		const tag = tagParts.length > 0 ? ` [${tagParts.join(' ')}]` : '';
		const firstLine = args.message.split('\n')[0]?.trim() ?? '';
		const ask = firstLine.length > 0 ? `Fix${tag}: ${firstLine}` : `Fix this${tag}`;
		const newDraft = `${ask} ${token}`;
		this.draft = this.draft.length === 0 ? newDraft : `${newDraft}\n\n${this.draft}`;
		this.composerFocusTick = this.composerFocusTick + 1;
	}

	/** Cap on a single pasted image. 4 MB is conservative across
	 *  providers — OpenAI tolerates 20 MB base64, Anthropic 5 MB,
	 *  HF Inference is squishier. We measure the decoded blob size
	 *  (not the base64 string), so the on-wire payload after
	 *  encoding lands a bit higher (~5.3 MB max) but still inside
	 *  every host's hard limit. Bigger images get a friendly
	 *  refusal instead of a silent provider 4xx. */
	static readonly IMAGE_MAX_BYTES = 4 * 1000 * 1000;
	/** Cap on simultaneous image attachments per send. Plenty for
	 *  any realistic "look at these screenshots" turn while still
	 *  bounding context-window blowups from accidental ten-paste
	 *  flurries. */
	static readonly MAX_IMAGE_ATTACHMENTS = 10;

	/** Add an image (typically from a clipboard paste) to the
	 *  composer's chip strip. Rejects oversized blobs and silently
	 *  ignores additions past [`MAX_IMAGE_ATTACHMENTS`] so the
	 *  user gets a stable cap rather than an unbounded queue. The
	 *  caller already has the bytes (paste handlers, drop
	 *  handlers); this method does the data-URL conversion in
	 *  one place. */
	async addImageAttachment(blob: Blob, name?: string): Promise<{ ok: true } | { ok: false; reason: string }> {
		if (blob.size === 0) {
			return { ok: false, reason: 'empty image' };
		}
		if (blob.size > CoderPanelState.IMAGE_MAX_BYTES) {
			const limitMb = Math.round(CoderPanelState.IMAGE_MAX_BYTES / 1_000_000);
			return { ok: false, reason: `image is ${formatBytes(blob.size)}; cap is ${limitMb} MB` };
		}
		const imageCount = this.attachments.filter((a) => a.kind === 'image').length;
		if (imageCount >= CoderPanelState.MAX_IMAGE_ATTACHMENTS) {
			return { ok: false, reason: `at most ${CoderPanelState.MAX_IMAGE_ATTACHMENTS} images per message` };
		}
		const mime = blob.type !== '' ? blob.type : 'image/png';
		const dataUrl = await blobToDataUrl(blob);
		rightPanel.set('coder');
		this.view = 'session';
		this.attachments = [
			...this.attachments,
			{
				kind: 'image',
				id: `img-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`,
				dataUrl,
				mime,
				name: name ?? `Image ${imageCount + 1}`,
				sizeBytes: blob.size,
			},
		];
		this.composerFocusTick = this.composerFocusTick + 1;
		return { ok: true };
	}

	removeAttachment(id: string): void {
		const att = this.attachments.find((a) => a.id === id);
		if (!att) {
			return;
		}
		this.attachments = this.attachments.filter((a) => a.id !== id);
		if (att.kind === 'image') {
			return;
		}
		// Strip every occurrence of the inline token (with at most
		// one trailing whitespace char) out of the draft. The user's
		// own typing might have nudged spacing around the token —
		// matching the token plus an optional `\s` keeps the most
		// common case clean without trying to be clever about
		// arbitrary punctuation. Applies equally to selection and
		// terminal attachments — both now carry an inline token, so
		// removing the chip removes its in-prose pointer.
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
			// composer before we reach here anyway. No DOM means no
			// undo to participate in either, so the direct draft
			// write is fine.
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
		// `execCommand('insertText')` is technically deprecated
		// but every webview we ship into (Chromium, WebKitGTK
		// via Tauri, WebView2) still implements it for textarea
		// inserts, and unlike a direct `value =` write it
		// *participates in the textarea's native undo stack* —
		// Ctrl+Z then pulls the token back out the same way it
		// reverses regular typing. The fallback below covers the
		// theoretical "execCommand returned false" path with a
		// direct draft write; native undo is lost there but the
		// token still lands.
		ta.focus();
		const ok = document.execCommand('insertText', false, insertion);
		if (!ok) {
			this.draft = `${before}${insertion}${after}`;
			const caret = before.length + insertion.length;
			queueMicrotask(() => {
				ta.selectionStart = caret;
				ta.selectionEnd = caret;
			});
		}
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
		// Wipe todos before replay; the session's last
		// `tool_result` for `todo_write` (if any) will repopulate
		// the bucket as the replay stream lands.
		this.current.todos = [];
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
			// Clear the per-folder token-usage / compaction state so
			// the context ring at the panel header doesn't carry
			// the previous session's prompt-tokens count into a
			// fresh blank session. The next `token_usage` event
			// repopulates the ring from zero.
			this.current.tokenUsage = null;
			this.current.compaction = null;
			// Same rationale for the todo list — a new session
			// starts with no plan.
			this.current.todos = [];
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
				// Same teardown as `newSession`: drop the deleted
				// session's token / compaction snapshot so the ring
				// doesn't outlive its data.
				this.current.tokenUsage = null;
				this.current.compaction = null;
				this.current.todos = [];
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
		// Clear any "agent finished, not seen yet" badge on the
		// folder we're switching to — the user is now looking
		// (or about to look). We only consult an existing bucket
		// rather than `bucketFor` so we don't lazy-create empty
		// buckets on every folder switch; a folder with no bucket
		// has no flag to clear by definition.
		if (path !== null) {
			const bucket = this.byFolder.get(path);
			if (bucket !== undefined && bucket.attentionPending) {
				bucket.attentionPending = false;
			}
			// Kick off first-time hydration for this folder. Gated
			// on `#workspaceReady` so the cold-start call from
			// `adoptWorkspaceSnapshot` doesn't race the folder-
			// restore loop (which is mid-flight at that point);
			// `markWorkspaceReady` flushes the active folder once
			// the loop is done.
			void this.#hydrateFolder(path);
		}
	}

	/** Bind the Tauri push-event listeners that drive the panel.
	 *  Idempotent — runs once per process; subsequent calls
	 *  early-return so HMR-driven re-mounts don't double-bind.
	 *
	 *  Deliberately separate from per-folder hydration
	 *  ([`hydrateActiveFolder`]). Listeners need to be live before
	 *  the first `coder:event` arrives (otherwise an in-flight turn
	 *  resumed across an HMR reload silently drops events) so this
	 *  fires early in `restoreAppState`. Hydration, in contrast,
	 *  reads through the backend's active-folder pointer and must
	 *  wait until the workspace folder-restore loop has stopped
	 *  mutating that pointer. */
	async wireRuntime(): Promise<void> {
		if (this.#listenersWired) {
			return;
		}
		this.#listenersWired = true;
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
	}

	/** Tell the panel that the backend's active folder is now
	 *  stable and per-folder hydration is safe to fire. Called by
	 *  `state.svelte.ts` after the `restoreAppState` folder-restore
	 *  loop has finished switching the backend's active-folder
	 *  pointer through every persisted folder. Idempotent.
	 *
	 *  Triggers an immediate hydrate for whichever folder is
	 *  currently active, and flushes any folder-switch hydrations
	 *  the user kicked off before the workspace was ready (those
	 *  are no-ops in `setActiveFolder` until the flag flips). */
	markWorkspaceReady(): void {
		if (this.#workspaceReady) {
			return;
		}
		this.#workspaceReady = true;
		if (this.activeFolderPath !== null) {
			void this.#hydrateFolder(this.activeFolderPath);
		}
	}

	/** Idempotent per-folder hydration. Once the workspace folder
	 *  loop has settled, calling this for the active folder runs
	 *  the initial `refreshStatus` + `#hydrateSession` pair that
	 *  used to live at the tail of [`wireRuntime`]. Safe to invoke
	 *  from both startup and folder-switch paths — repeat calls
	 *  for an already-hydrated folder return immediately, so
	 *  switching back to a folder doesn't re-fetch its sessions
	 *  list (per the multi-session "switching folders doesn't
	 *  re-hydrate" contract). */
	async #hydrateFolder(path: string): Promise<void> {
		if (!this.#workspaceReady) {
			return;
		}
		if (this.#hydratedFolders.has(path)) {
			return;
		}
		this.#hydratedFolders.add(path);
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
					// Call the IPC directly rather than going
					// through `this.openSession(id)`. The latter
					// has its own try/catch that surfaces failures
					// as an inline error row — correct for an
					// explicit user click, but wrong for silent
					// hydration: the row would mount with the
					// error already in `rows` and the user would
					// have to back out manually to recover. Direct
					// IPC lets the error bubble to the catch below
					// so we fall through to the sessions list
					// instead. The state-reset that `openSession`
					// does before its IPC call is a no-op here
					// because the bucket is freshly initialised at
					// hydration time.
					const summary = await ipc.coder.openSession(id);
					this.activeSession = summary;
					this.view = 'session';
					return;
				} catch {
					// Stale pointer — the session no longer exists
					// on disk (manual rm, dev-cleanup of
					// `~/.local/share/moon-ide/`, etc.). Fall
					// through to the list view; the pointer will
					// be overwritten the next time the user opens
					// or sends in a session.
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
		this.current.todos = [];
		await this.refreshStatus();
	}

	async send(activeFilePath: string | null = null): Promise<void> {
		const text = this.draft.trim();
		const attachments = this.attachments;
		// Allow sending when *any* of text, selections, or images
		// is present — "explain this" with an attachment but no
		// question is a perfectly reasonable starter. The
		// active-file hint is implicit: present-or-absent on every
		// turn, doesn't count as "the user wanted to send" on its
		// own (it would auto-fire on Enter in an empty composer).
		// Sending while busy is a *steer*: the backend queues it
		// and drains into the running turn at its next iteration
		// boundary so the model can incorporate the new context
		// without the user having to abort and restart.
		if (text.length === 0 && attachments.length === 0) {
			return;
		}
		const selectionAttachments: SelectionAttachment[] = [];
		const imageAttachments: ImageComposerAttachment[] = [];
		const terminalAttachments: TerminalAttachment[] = [];
		for (const att of attachments) {
			if (att.kind === 'selection') {
				selectionAttachments.push(att);
			} else if (att.kind === 'image') {
				imageAttachments.push(att);
			} else {
				terminalAttachments.push(att);
			}
		}
		const payload = renderPromptWithAttachments(text, selectionAttachments, terminalAttachments, activeFilePath);
		const images: ImageAttachmentPayload[] = imageAttachments.map((img) => ({
			data_url: img.dataUrl,
			mime: img.mime,
		}));
		this.draft = '';
		this.clearAttachments();
		// Optimistic flip — the `user_message` event lands within
		// milliseconds and reconciles. For an initial send this is
		// what stops a double-fire while the IPC is in flight; for
		// a steer (already busy) it's a no-op.
		this.busy = true;
		try {
			await ipc.coder.send(payload, images);
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

	/** Pop the most recently queued steer from the active folder's
	 *  transcript back into the composer.
	 *
	 *  Bound to `ArrowUp` on an empty composer in the panel — the
	 *  user just typed a steer mid-turn, realised they want to
	 *  edit it before it lands in the chat, and presses Up to
	 *  pull it back. The empty-composer guard keeps the regular
	 *  textarea Up-arrow behaviour intact for everything else.
	 *  We:
	 *
	 *  1. Find the latest `queued: true` user row in the active
	 *     bucket (queued rows are always tail-end of the
	 *     transcript, since the runner emits `UserMessage` as
	 *     soon as the steer is queued).
	 *  2. Call `coder_unqueue_steer(id)`. The runner removes the
	 *     matching `PendingSteer` and emits `steer_drained` so
	 *     the row's queued style flips off in lockstep with the
	 *     transcript edit below — handy for sibling windows.
	 *  3. Restore the original text to the draft, push the
	 *     original images back as composer chips, drop the row
	 *     from the transcript, and focus the composer.
	 *
	 *  Returns `true` when something was actually unqueued.
	 *  `false` when there's no queued steer to pop, the IPC
	 *  reported the steer had already been drained (race against
	 *  the runner's iteration-top drain), or the call failed —
	 *  the caller (the `Ctrl+Up` handler) treats `false` as
	 *  "let the default arrow-key behaviour happen". */
	async unqueueLatestSteer(): Promise<boolean> {
		const latestQueued = this.rows
			.toReversed()
			.find((row): row is Extract<CoderRow, { kind: 'user' }> => row.kind === 'user' && row.queued);
		if (!latestQueued) {
			return false;
		}
		let popped: { text: string; images?: ImageAttachmentPayload[] } | null;
		try {
			popped = await ipc.coder.unqueueSteer(latestQueued.id);
		} catch (err) {
			// Surface inside the transcript rather than as a host
			// flash — the user is mid-conversation and the rest of
			// the panel's errors already land here.
			this.rows = [
				...this.rows,
				{
					kind: 'error',
					id: `local-${Date.now()}`,
					text: `Failed to un-queue message: ${formatError(err)}`,
				},
			];
			return false;
		}
		if (popped === null) {
			// Backend says the steer was already drained — flip
			// the local row's flag to match (the `steer_drained`
			// event would have done this too, but it may not have
			// arrived yet) and leave the draft alone.
			this.rows = this.rows.map((row) =>
				row.kind === 'user' && row.id === latestQueued.id ? { ...row, queued: false } : row,
			);
			return false;
		}
		this.rows = this.rows.filter((row) => !(row.kind === 'user' && row.id === latestQueued.id));
		// Splice the original text into wherever the caret was —
		// usually at offset 0 since the user pressed Up on an
		// empty composer, but be tolerant of "they started typing
		// while we were awaiting the IPC". A trailing space keeps
		// the keep-typing flow natural when the draft already had
		// suffix text.
		const restoredText = popped.text;
		const beforeDraft = this.draft;
		this.draft = beforeDraft.length === 0 ? restoredText : `${restoredText} ${beforeDraft}`;
		// Push the images back onto the chip strip. We lost the
		// original `name` / `sizeBytes` (they weren't shipped to
		// the backend) — reconstruct a reasonable display name and
		// approximate the size from the base64 payload so the chip
		// reads sensibly.
		const images = popped.images ?? [];
		const restoredImages: ImageComposerAttachment[] = images.map((img, idx) => ({
			kind: 'image',
			id: `img-${Date.now()}-${Math.random().toString(36).slice(2, 8)}-${idx}`,
			dataUrl: img.data_url,
			mime: img.mime,
			name: `Image ${idx + 1}`,
			sizeBytes: approximateBase64Size(img.data_url),
		}));
		if (restoredImages.length > 0) {
			this.attachments = [...this.attachments, ...restoredImages];
		}
		this.composerFocusTick = this.composerFocusTick + 1;
		return true;
	}

	/** `true` when the active bucket has at least one queued steer
	 *  the user can un-queue with `ArrowUp` on an empty composer.
	 *  Hot path — the panel reads this every key press, so we
	 *  keep it a cheap scan. */
	get hasQueuedSteer(): boolean {
		for (const row of this.rows) {
			if (row.kind === 'user' && row.queued) {
				return true;
			}
		}
		return false;
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
	/** Flip a bucket's `attentionPending` flag to `true` iff this
	 *  is a *background* completion — i.e. a turn that ended
	 *  while the user was looking at a different folder (or no
	 *  folder at all). An active-folder turn-end doesn't need
	 *  the badge: the user is already on the panel, the result
	 *  is already on screen, and lighting up an "agent finished"
	 *  cue would just be visual noise.
	 *
	 *  The `NO_FOLDER_KEY` guard suppresses the badge for the
	 *  pre-bind sentinel bucket: a turn that completed before
	 *  any folder was active can't be associated with a folder
	 *  bar to render on, so the flag would be dead state. */
	#flagAttentionIfBackground(bucket: FolderViewState, folder: string): void {
		if (folder === NO_FOLDER_KEY) {
			return;
		}
		const active = this.activeFolderPath ?? NO_FOLDER_KEY;
		if (folder === active) {
			return;
		}
		bucket.attentionPending = true;
	}

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
				bucket.rows = [
					...bucket.rows,
					{
						kind: 'user',
						id: event.id,
						text: event.text,
						images: event.images ?? [],
						queued: event.queued ?? false,
					},
				];
				bucket.busy = true;
				return;
			case 'steer_drained':
				// Runner moved (or `coder.unqueueSteer` popped) the
				// queued message; flip the row out of "queued"
				// styling. Idempotent — a duplicate event lands as
				// a no-op (the row is already `queued: false`).
				bucket.rows = bucket.rows.map((row) =>
					row.kind === 'user' && row.id === event.id ? { ...row, queued: false } : row,
				);
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
				// Mirror the canonical post-merge list from a
				// successful `todo_write` into the bucket so the
				// header pill / popover stay in lock-step with the
				// model. Errored calls are skipped — the list
				// hasn't actually changed in that case (the runner
				// short-circuits before mutating
				// `Session.todos`). The match keys off the parent
				// row's tool name; we don't have the name on the
				// `tool_result` event itself. Replay re-emits the
				// same `tool_call` + `tool_result` pair so this
				// path also seeds the bucket on session reopen.
				if (!event.is_error) {
					const parent = bucket.rows.find((row) => row.kind === 'tool' && row.id === event.id);
					if (parent && parent.kind === 'tool' && parent.name === 'todo_write') {
						const next = extractTodos(event.result);
						if (next !== null) {
							bucket.todos = next;
						}
					}
				}
				return;
			case 'turn_complete':
				bucket.busy = false;
				this.#flagAttentionIfBackground(bucket, folder);
				return;
			case 'aborted':
				bucket.busy = false;
				bucket.rows = [...bucket.rows, { kind: 'aborted', id: `aborted-${Date.now()}` }];
				this.#flagAttentionIfBackground(bucket, folder);
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
				this.#flagAttentionIfBackground(bucket, folder);
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
				// Wipe the todo list before replay; the session's
				// last `tool_result` for `todo_write` (if any)
				// repopulates this in the per-record replay stream.
				bucket.todos = [];
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
					cacheReadTokens: event.cache_read_tokens,
					cacheCreationTokens: event.cache_creation_tokens,
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
			case 'hub_sync_started':
				this.hubSyncState = {
					...this.hubSyncState,
					[event.session_id]: { phase: 'syncing' },
				};
				return;
			case 'hub_sync_finished': {
				this.hubSyncState = {
					...this.hubSyncState,
					[event.session_id]: event.ok
						? { phase: 'synced', atMs: Date.now() }
						: { phase: 'failed', error: event.error ?? 'Upload failed' },
				};
				if (event.ok) {
					this.loadHubBinding().catch(() => {});
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
 *  works.
 *
 *  `activeFilePath` is the path of whichever file the user has
 *  focused in the editor at send time, or `null` when nothing
 *  routable is open (terminal-only focus, untitled buffer,
 *  external host-direct buffer, file the user deleted). The hint
 *  ships every turn the user has something open — it's cheap
 *  (~30 tokens), survives compaction (older turns reduce to a
 *  summary; the model still needs "what's open *now*" from this
 *  turn), and gives the model enough to call `read_file` on its
 *  own for follow-ups like "explain this" or "add a test for the
 *  function I'm looking at" without the user needing `Ctrl+L` for
 *  context-free questions. We deliberately do **not** ship the
 *  file's contents implicitly — that's still `Ctrl+L`'s job.
 *  Per-turn (in the user message), not in the system prompt, so
 *  tab switches don't bust the router's prefix cache. */
function renderPromptWithAttachments(
	text: string,
	attachments: SelectionAttachment[],
	terminalAttachments: TerminalAttachment[],
	activeFilePath: string | null,
): string {
	const blocks: string[] = [];
	if (activeFilePath !== null) {
		// Self-closing — the element is metadata, not content.
		// The renderer's `parseUserPrompt` only chip-strips
		// `<code_selection>` elements, so `<active_file>` rides
		// through invisibly in the UI while still reaching the
		// model.
		blocks.push(`<active_file path="${escapeXmlAttr(activeFilePath)}" />`);
	}
	for (const att of attachments) {
		const range = att.startLine === att.endLine ? `${att.startLine}` : `${att.startLine}-${att.endLine}`;
		// Wrap the captured text verbatim. We don't fence the body
		// since the surrounding `<code_selection>` element is
		// already an unambiguous delimiter — no risk of
		// triple-backticks in the snippet "closing" our wrapper.
		blocks.push(`<code_selection path="${escapeXmlAttr(att.path)}" lines="${range}">\n${att.text}\n</code_selection>`);
	}
	for (const att of terminalAttachments) {
		// Same envelope strategy: the `<terminal_output>` tag is
		// the delimiter, no fenced body. `label` is the human
		// terminal title (cwd basename in practice); `token`
		// echoes the inline `@terminal:<label>` reference spliced
		// into the draft so the model can correlate the in-prose
		// pointer with the matching context element when the user
		// attached output from several terminals (or several
		// disjoint snippets from one).
		blocks.push(
			`<terminal_output token="${escapeXmlAttr(att.token)}" label="${escapeXmlAttr(att.label)}">\n${att.text}\n</terminal_output>`,
		);
	}
	if (blocks.length === 0) {
		return text;
	}
	const context = `<context>\n${blocks.join('\n')}\n</context>`;
	return text.length > 0 ? `${text}\n\n${context}` : context;
}

const REGEX_META_PATTERN = /[\\^$.*+?()[\]{}|]/g;

function escapeRegExp(s: string): string {
	return s.replace(REGEX_META_PATTERN, '\\$&');
}

/** Read a blob as a `data:<mime>;base64,...` URL. Used for paste
 *  / drop image attachments — `FileReader.readAsDataURL` is the
 *  one-shot path that handles every browser-supported image MIME
 *  without us having to base64-encode bytes ourselves. */
function blobToDataUrl(blob: Blob): Promise<string> {
	return new Promise((resolve, reject) => {
		const reader = new FileReader();
		reader.addEventListener(
			'load',
			() => {
				const result = reader.result;
				if (typeof result !== 'string') {
					reject(new Error('FileReader returned non-string result for a blob'));
					return;
				}
				resolve(result);
			},
			{ once: true },
		);
		reader.addEventListener('error', () => reject(reader.error ?? new Error('FileReader failed')), { once: true });
		reader.readAsDataURL(blob);
	});
}

/** Estimate the raw byte size of a `data:<mime>;base64,...` URL
 *  without decoding it. Base64 expands 3 source bytes to 4
 *  characters and pads the tail with `=`, so the inverse is
 *  `floor(base64_len * 3 / 4) - padding`. Used on the un-queue
 *  path where the original `sizeBytes` was never sent to the
 *  backend; close enough for the chip's "Image (97 kB)" hint. */
function approximateBase64Size(dataUrl: string): number {
	const commaIdx = dataUrl.indexOf(',');
	if (commaIdx === -1) {
		return 0;
	}
	const body = dataUrl.slice(commaIdx + 1);
	const padding = body.endsWith('==') ? 2 : body.endsWith('=') ? 1 : 0;
	return Math.max(0, Math.floor((body.length * 3) / 4) - padding);
}

/** Pretty-print a byte count for the "image too large" error
 *  message. We use 1000-multiples per house style. */
function formatBytes(n: number): string {
	if (n < 1000) {
		return `${n} B`;
	}
	if (n < 1_000_000) {
		return `${(n / 1000).toFixed(1)} kB`;
	}
	return `${(n / 1_000_000).toFixed(1)} MB`;
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

/** Squeeze a terminal tab title into something safe to use as the
 *  body of an `@terminal:<label>` token. Whitespace becomes `-`,
 *  any non-word/dot/dash byte is dropped, and a blank result falls
 *  back to `terminal` so the token always has a body the model
 *  can read as a single word. */
function sanitiseTokenLabel(label: string): string {
	const collapsed = label.trim().replace(/\s+/g, '-');
	const stripped = collapsed.replace(/[^\w.-]/g, '');
	return stripped.length > 0 ? stripped : 'terminal';
}

/** Count newline-separated lines in a text blob, treating an
 *  empty trailing newline as part of the last line (so "a\nb\n"
 *  is 2 lines, not 3). Used for terminal-attachment chip labels
 *  — the user wants "5 lines", not "6 lines because the shell
 *  echoes a final \n". */
function countLines(text: string): number {
	if (text.length === 0) {
		return 0;
	}
	const trimmed = text.endsWith('\n') ? text.slice(0, -1) : text;
	let count = 1;
	for (const ch of trimmed) {
		if (ch === '\n') {
			count += 1;
		}
	}
	return count;
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
