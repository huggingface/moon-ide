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
	type CoderSessionSummary,
	type CoderStatus,
	type DeviceCode,
	type HfIdentity,
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
 *  land, auto-collapsed on `assistant_message_end`. */
export type CoderRow =
	| { kind: 'user'; id: string; text: string }
	| { kind: 'assistant'; id: string; text: string; thinking: string; thinkingOpen: boolean }
	| { kind: 'tool'; id: string; name: string; args: unknown; result: unknown; hasResult: boolean; isError: boolean }
	| { kind: 'error'; id: string; text: string }
	| { kind: 'aborted'; id: string };

/** Which view of the Coder panel is mounted. `'list'` shows the
 *  sessions list (mirrors the Slack panel's "← Sessions" gesture);
 *  `'session'` shows an active session's transcript + composer. */
export type CoderView = 'list' | 'session';

class CoderPanelState {
	/** Whether the right-side slot is currently mounted with the
	 *  coder surface. Derived from the shared `rightPanel.kind` —
	 *  chat and coder share one slot. */
	get panelVisible(): boolean {
		return rightPanel.kind === 'coder';
	}

	/** Latest `coder_status`. `null` before the first call. */
	status = $state<CoderStatus | null>(null);

	/** Sessions list snapshot, refreshed on `session_list_changed`
	 *  or after `coder_open_session` / `coder_delete_session`.
	 *  `null` until the first fetch lands so the UI can show a
	 *  loading state vs. "no sessions yet". */
	sessions = $state<CoderSessionSummary[] | null>(null);

	/** Metadata for the session currently mounted in memory. `null`
	 *  for a fresh session that hasn't received its first user
	 *  message yet — the panel renders the "send a prompt to
	 *  start" state in that case. */
	activeSession = $state<CoderSessionSummary | null>(null);

	/** Which view of the panel to render — sessions list vs
	 *  transcript. Defaults to `'session'` so a relaunch with a
	 *  remembered session id lands the user back in their last
	 *  conversation; the panel switches to `'list'` when the user
	 *  hits "← Sessions". */
	view = $state<CoderView>('session');

	/** Active device-flow code, while the connect modal is open. */
	deviceCode = $state<DeviceCode | null>(null);

	/** UI flag while `coder_start_device_flow` is in flight. */
	startingFlow = $state(false);

	/** UI flag while we're polling the token endpoint. */
	awaitingApproval = $state(false);

	/** Latest sign-in error (device-flow expired, denied, network). */
	signInError = $state<string | null>(null);

	/** Whether a turn is currently running locally — drives the
	 *  composer disable + the stop-button visibility. The backend's
	 *  authoritative `busy` flag lands via `coder_status`; we keep a
	 *  derived local copy that flips immediately on `send`/event so
	 *  the UI doesn't lag. */
	busy = $state(false);

	/** Transcript rows in display order. Cleared on sign-out. */
	rows = $state<CoderRow[]>([]);

	/** Current composer draft. Frontend-only — not persisted in
	 *  6.0 because the session itself isn't persisted. */
	draft = $state('');

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
			this.busy = next.busy;
		} catch {
			// Status probe failures are silent: the panel still
			// renders the empty state and the next user action
			// (sign-in attempt, send) will surface the real error.
		}
	}

	async wireRuntime(): Promise<void> {
		if (this.#runtimeWired) {
			return;
		}
		this.#runtimeWired = true;
		try {
			const unlisten = await listen<CoderEvent>(CODER_EVENT_CHANNEL, (event) => {
				this.#applyEvent(event.payload);
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

	/** First-mount session hydration. Tries to restore the active
	 *  session the runner already has (in dev, HMR keeps it
	 *  across reloads); if there's none, falls back to the
	 *  remembered `last_session_id` from `AppState`; if that's
	 *  also missing or no longer exists in the active folder,
	 *  shows the sessions list view. Best-effort throughout. */
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
			const id = appState.coder.last_session_id;
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
		this.busy = false;
		await this.refreshStatus();
	}

	async send(): Promise<void> {
		const text = this.draft.trim();
		if (text.length === 0 || this.busy) {
			return;
		}
		this.draft = '';
		// Optimistic flip — the `user_message` event lands within
		// milliseconds and reconciles, but the composer needs to
		// disable immediately or the user can fire a second turn
		// before the round-trip completes.
		this.busy = true;
		try {
			await ipc.coder.send(text);
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

	#applyEvent(event: CoderEvent): void {
		switch (event.kind) {
			case 'user_message':
				this.rows = [...this.rows, { kind: 'user', id: event.id, text: event.text }];
				this.busy = true;
				return;
			case 'assistant_message_start':
				// Insert the empty bubble so the user sees the row
				// land instantly, even before the model emits its
				// first token. Idempotent: the runner only fires
				// `start` once per id, but we'd no-op a duplicate
				// rather than insert a phantom row.
				if (this.rows.some((r) => r.kind === 'assistant' && r.id === event.id)) {
					return;
				}
				this.rows = [...this.rows, { kind: 'assistant', id: event.id, text: '', thinking: '', thinkingOpen: true }];
				return;
			case 'assistant_message_delta':
				this.rows = appendDelta(this.rows, event.id, event.delta, 'text');
				return;
			case 'assistant_thinking_delta':
				this.rows = appendDelta(this.rows, event.id, event.delta, 'thinking');
				return;
			case 'assistant_message_end':
				// Canonical replacement at close — see the file
				// header for the rationale (drift between
				// concatenated deltas and the final assembly heals
				// on close, plus markdown rendering re-runs once on
				// the complete text). Auto-collapse the thinking
				// block: the user already saw it stream, the answer
				// is the takeaway.
				this.rows = this.rows.map((row) =>
					row.kind === 'assistant' && row.id === event.id
						? { ...row, text: event.text, thinking: event.thinking ?? row.thinking, thinkingOpen: false }
						: row,
				);
				return;
			case 'tool_call':
				this.rows = [
					...this.rows,
					{
						kind: 'tool',
						id: event.id,
						name: event.name,
						args: event.args,
						result: undefined,
						hasResult: false,
						isError: false,
					},
				];
				return;
			case 'tool_result':
				this.rows = this.rows.map((row) =>
					row.kind === 'tool' && row.id === event.id
						? { ...row, result: event.result, hasResult: true, isError: event.is_error }
						: row,
				);
				return;
			case 'turn_complete':
				this.busy = false;
				return;
			case 'aborted':
				this.busy = false;
				this.rows = [...this.rows, { kind: 'aborted', id: `aborted-${Date.now()}` }];
				return;
			case 'error':
				this.busy = false;
				this.rows = [
					...this.rows,
					{
						kind: 'error',
						id: `error-${Date.now()}`,
						text: event.message,
					},
				];
				return;
			case 'session_loaded':
				// Reset the transcript and adopt the new session's
				// metadata. Replay events arrive immediately after
				// this one (fired by the backend on the same
				// `coder:event` channel), so the rows fill in on
				// the next handlers.
				this.rows = [];
				this.busy = false;
				this.activeSession = {
					id: event.id,
					title: event.title,
					created_at_ms: event.created_at_ms,
					updated_at_ms: event.updated_at_ms,
				};
				this.view = 'session';
				return;
			case 'session_title_updated':
				if (this.activeSession?.id === event.id) {
					this.activeSession = { ...this.activeSession, title: event.title };
				}
				if (this.sessions !== null) {
					this.sessions = this.sessions.map((s) => (s.id === event.id ? { ...s, title: event.title } : s));
				}
				return;
			case 'session_list_changed':
				void this.refreshSessions();
				return;
		}
	}
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
