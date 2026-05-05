//! Reactive state for the Coder panel.
//!
//! Phase 6.0 surface: device-flow sign-in, single in-memory session,
//! send / abort. The panel rebuilds its message list from the
//! `coder:event` Tauri stream — there's no persistence layer behind
//! it yet (lands in 6.3). A page reload therefore loses the visible
//! transcript; the loop's own session memory survives because it
//! lives in the Rust process.
//!
//! See `specs/coder.md` and `specs/test-plans/0039-coder-skeleton.md`.

import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import { ipc } from './ipc';
import { formatError, type CoderEvent, type CoderStatus, type DeviceCode, type HfIdentity } from './protocol';

const CODER_EVENT_CHANNEL = 'coder:event';

/** One row rendered in the panel transcript. The `kind` matches the
 *  loop event that produced it; the `id` is stable so the runner's
 *  `tool_call` → `tool_result` pair update the same DOM node when
 *  the tool finishes. */
export type CoderRow =
	| { kind: 'user'; id: string; text: string }
	| { kind: 'assistant'; id: string; text: string }
	| { kind: 'tool'; id: string; name: string; args: unknown; result: unknown; hasResult: boolean; isError: boolean }
	| { kind: 'error'; id: string; text: string }
	| { kind: 'aborted'; id: string };

class CoderPanelState {
	/** Whether the right-side coder panel is currently rendered. */
	panelVisible = $state(false);

	/** Latest `coder_status`. `null` before the first call. */
	status = $state<CoderStatus | null>(null);

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
		this.panelVisible = !this.panelVisible;
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
		await this.refreshStatus();
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
			case 'assistant_message':
				this.rows = [...this.rows, { kind: 'assistant', id: event.id, text: event.text }];
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
		}
	}
}

export const coder = new CoderPanelState();
