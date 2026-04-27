//! Reactive state for the Slack chat panel.
//!
//! Phase 11.0 models connect / disconnect / bot picker — sessions,
//! threads, and message lists join in 11.1+. Kept in its own file
//! (rather than bolted onto `WorkspaceState`) because the chat panel's
//! lifecycle is independent of the workspace: it survives "open
//! another folder", and a future "no folder open, just chatting" flow
//! doesn't need the workspace to exist at all.

import { ipc } from './ipc';
import { formatError, type SlackBotProfile, type SlackIdentity, type SlackStatus } from './protocol';

export type ConnectResult = { ok: true; identity: SlackIdentity } | { ok: false; error: string };

class SlackPanelState {
	/** Whether the right-side panel is currently rendered. */
	panelVisible = $state(false);

	/** Last result of `slack_status`. `null` before the first poll. */
	status = $state<SlackStatus | null>(null);

	/**
	 * Bot the user has picked (and the backend has persisted to
	 * `app_state.json`). `null` means the picker should appear.
	 */
	activeBot = $state<SlackBotProfile | null>(null);

	/** Bot candidates from the most recent DM scan. */
	botCandidates = $state<SlackBotProfile[]>([]);

	/** UI flag: set while `slack_set_token` is in flight. */
	connecting = $state(false);

	/** UI flag: set while `slack_list_dm_bots` is in flight. */
	loadingBots = $state(false);

	/**
	 * Latest discovery error. Cleared when a scan succeeds (even with
	 * zero results) so the UI distinguishes "no bots found" from
	 * "scan failed".
	 */
	botError = $state<string | null>(null);

	/** Whether the "Connect Slack" modal is mounted. */
	showConnectModal = $state(false);

	get connected(): boolean {
		return this.status?.connected ?? false;
	}

	togglePanel() {
		this.panelVisible = !this.panelVisible;
		if (this.panelVisible && this.status === null) {
			void this.refreshStatus();
		}
	}

	openConnectModal() {
		this.showConnectModal = true;
	}

	closeConnectModal() {
		this.showConnectModal = false;
	}

	async refreshStatus(): Promise<void> {
		try {
			const status = await ipc.slack.status();
			this.status = status;
			if (!status.connected) {
				this.activeBot = null;
				this.botCandidates = [];
				this.botError = null;
				return;
			}
			// Connected. Reload the persisted bot pick first; only fall
			// back to the picker (DM scan) when there isn't one. A
			// failure here is non-fatal — we just treat it as "no saved
			// bot" and run the picker. Failures of this exact call only
			// happen when `app_state.json` is corrupt, which we already
			// recover from elsewhere.
			if (this.activeBot === null) {
				try {
					this.activeBot = await ipc.slack.getActiveBot();
				} catch {
					// fall through to the picker
				}
			}
			if (this.activeBot === null && !this.loadingBots) {
				void this.discoverBots();
			}
		} catch (err) {
			this.status = { connected: false, identity: null };
			this.activeBot = null;
			this.botCandidates = [];
			this.botError = formatError(err);
		}
	}

	async connect(token: string): Promise<ConnectResult> {
		this.connecting = true;
		try {
			const identity = await ipc.slack.setToken(token);
			this.status = { connected: true, identity };
			this.activeBot = null;
			this.botCandidates = [];
			this.botError = null;
			void this.discoverBots();
			return { ok: true, identity };
		} catch (err) {
			return { ok: false, error: formatError(err) };
		} finally {
			this.connecting = false;
		}
	}

	async disconnect(): Promise<void> {
		try {
			await ipc.slack.clearToken();
		} catch {
			// Clearing fails only if the keyring backend itself is dead;
			// we still want to drop in-memory state so the UI shows the
			// empty state immediately. The next launch will retry the
			// keyring delete via slack_status.
		}
		this.status = { connected: false, identity: null };
		this.activeBot = null;
		this.botCandidates = [];
		this.botError = null;
	}

	/**
	 * Scan the user's DMs for bots and populate `botCandidates`. The
	 * call is slow on large workspaces (one `users.info` per DM); the
	 * chat panel renders a spinner while `loadingBots` is true.
	 */
	async discoverBots(): Promise<void> {
		this.loadingBots = true;
		this.botError = null;
		try {
			this.botCandidates = await ipc.slack.listDmBots();
		} catch (err) {
			this.botCandidates = [];
			this.botError = formatError(err);
		} finally {
			this.loadingBots = false;
		}
	}

	/**
	 * Persist the user's pick. Subsequent launches skip the picker and
	 * jump straight to the active-bot card.
	 */
	async selectBot(profile: SlackBotProfile): Promise<void> {
		this.activeBot = profile;
		try {
			await ipc.slack.selectBot(profile);
		} catch (err) {
			// Persistence failed but we keep the in-memory selection so
			// the user can keep working this session. Surface the error
			// so they know to retry on next connect.
			this.botError = formatError(err);
		}
	}

	/**
	 * Drop the active bot pick. Returns the panel to the picker on the
	 * next render and triggers a fresh DM scan.
	 */
	async clearBotSelection(): Promise<void> {
		this.activeBot = null;
		try {
			await ipc.slack.clearBot();
		} catch (err) {
			this.botError = formatError(err);
		}
		void this.discoverBots();
	}
}

export const slack = new SlackPanelState();
