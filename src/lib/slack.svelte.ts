//! Reactive state for the Slack chat panel.
//!
//! Phases 11.0–11.1 cover: connect / disconnect, bot picker,
//! sessions list (top-level DM messages), and active thread (read-only
//! message bubbles). Polling, edits, sending join in 11.2+. Kept in
//! its own file (rather than bolted onto `WorkspaceState`) because the
//! chat panel's lifecycle is independent of the workspace: it
//! survives "open another folder", and a future "no folder open, just
//! chatting" flow doesn't need the workspace to exist at all.

import { SvelteMap } from 'svelte/reactivity';

import { ipc } from './ipc';
import {
	formatError,
	type SlackAppState,
	type SlackBotProfile,
	type SlackIdentity,
	type SlackMessage,
	type SlackSession,
	type SlackStatus,
	type SlackUserSummary,
} from './protocol';

export type ConnectResult = { ok: true; identity: SlackIdentity } | { ok: false; error: string };

/**
 * Reactive cache entry for one Slack user. The mrkdwn renderer reads
 * the entry, sees `loading` and renders a placeholder pill, then
 * automatically re-renders when the cache transitions to `resolved`.
 *
 * `missing` is its own state (not just `null`): it lets the renderer
 * fall back to the raw user_id without infinitely retrying a 404.
 */
export type SlackUserCacheEntry =
	| { state: 'loading' }
	| { state: 'resolved'; user: SlackUserSummary }
	| { state: 'missing' };

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

	/**
	 * Top-level DM messages with the active bot, newest-first. `null`
	 * before the first load. The frontend never paginates — see the
	 * `SESSION_HISTORY_LIMIT` cap in `moon-slack`.
	 */
	sessions = $state<SlackSession[] | null>(null);

	/** UI flag while `slack_list_sessions` is in flight. */
	loadingSessions = $state(false);

	/** Latest sessions error; cleared on success (incl. zero results). */
	sessionsError = $state<string | null>(null);

	/**
	 * `thread_ts` of the session the user is currently reading. `null`
	 * means "show the session list, no thread open". Persisted in
	 * `AppState.slack.active_thread_ts`.
	 */
	activeThreadTs = $state<string | null>(null);

	/**
	 * Messages of the active thread (parent + replies, oldest-first).
	 * `null` before the first load — the panel renders a "Loading
	 * thread…" affordance while we fetch.
	 */
	threadMessages = $state<SlackMessage[] | null>(null);

	/** UI flag while `slack_get_thread` is in flight. */
	loadingThread = $state(false);

	/** Latest thread error; cleared on success. */
	threadError = $state<string | null>(null);

	/**
	 * Generation counters used to discard late-arriving network
	 * responses when the user has moved on (different bot, different
	 * thread). The frontend reads `current === captured` after each
	 * await before mutating reactive state.
	 */
	#sessionsGeneration = 0;
	#threadGeneration = 0;

	/**
	 * Resolved-name cache for `<@U…>` mention rendering. Reactive so
	 * placeholders auto-upgrade to the resolved label without the
	 * renderer needing to re-trigger a load. Keyed by `user_id`. Never
	 * cleared at runtime — Slack workspace user IDs don't churn, and
	 * the cache lives only as long as the process. Disconnect resets
	 * it via [`#resetUserCache`].
	 */
	userCache = new SvelteMap<string, SlackUserCacheEntry>();

	get connected(): boolean {
		return this.status?.connected ?? false;
	}

	/**
	 * Apply persisted panel state at startup. Called once from
	 * `WorkspaceState.restoreAppState`. Pre-loads `activeBot` from disk
	 * so the chat panel's first paint already shows the active-bot card
	 * (skipping the spinner + DM scan that `refreshStatus` would
	 * otherwise kick off). Panel visibility is restored verbatim — if
	 * the user had the chat panel open last session, it stays open.
	 */
	hydrate(state: SlackAppState) {
		this.activeBot = state.active_bot;
		this.panelVisible = state.panel_visible;
		this.activeThreadTs = state.active_thread_ts;
		this.#seedActiveBot();
	}

	togglePanel() {
		this.setPanelVisible(!this.panelVisible);
	}

	setPanelVisible(visible: boolean) {
		if (this.panelVisible === visible) {
			return;
		}
		this.panelVisible = visible;
		if (this.panelVisible && this.status === null) {
			void this.refreshStatus();
		}
		// Persistence is fire-and-forget — a failed write only means the
		// panel forgets its state on the next launch, which is at worst
		// mildly annoying. The chat panel itself still works.
		void ipc.slack.setPanelVisible(visible).catch(() => {});
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
				this.#resetThreadView();
				return;
			}
			this.#seedSelf();
			// Connected. Reload the persisted bot pick first; only fall
			// back to the picker (DM scan) when there isn't one. A
			// failure here is non-fatal — we just treat it as "no saved
			// bot" and run the picker. Failures of this exact call only
			// happen when `app_state.json` is corrupt, which we already
			// recover from elsewhere.
			if (this.activeBot === null) {
				try {
					this.activeBot = await ipc.slack.getActiveBot();
					this.#seedActiveBot();
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
			this.#seedSelf();
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
		this.#resetThreadView();
		this.#resetUserCache();
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
	 * jump straight to the active-bot card. Switching bots invalidates
	 * the session list and active thread — they live inside the
	 * previous bot's DM channel, so the new bot starts fresh.
	 */
	async selectBot(profile: SlackBotProfile): Promise<void> {
		const previous = this.activeBot;
		this.activeBot = profile;
		this.#seedActiveBot();
		if (previous?.user_id !== profile.user_id) {
			this.#resetThreadView();
		}
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
		this.#resetThreadView();
		try {
			await ipc.slack.clearBot();
		} catch (err) {
			this.botError = formatError(err);
		}
		void this.discoverBots();
	}

	/**
	 * Load the session list for the active bot's DM channel. Cheap to
	 * call repeatedly — the panel's `onMount` triggers it after every
	 * status / bot change. Late responses for a stale bot are
	 * discarded via the generation counter so a fast bot-switch
	 * doesn't stomp the new bot's state with the old one's.
	 */
	async loadSessions(): Promise<void> {
		const bot = this.activeBot;
		if (bot === null) {
			return;
		}
		this.loadingSessions = true;
		this.sessionsError = null;
		const generation = ++this.#sessionsGeneration;
		const captured = bot.user_id;
		try {
			const sessions = await ipc.slack.listSessions(bot.dm_channel_id);
			if (generation !== this.#sessionsGeneration || captured !== this.activeBot?.user_id) {
				return;
			}
			this.sessions = sessions;
		} catch (err) {
			if (generation !== this.#sessionsGeneration || captured !== this.activeBot?.user_id) {
				return;
			}
			this.sessions = [];
			this.sessionsError = formatError(err);
		} finally {
			if (generation === this.#sessionsGeneration) {
				this.loadingSessions = false;
			}
		}
	}

	/**
	 * Open a thread (or `null` to return to the session list). Persists
	 * the pick so it survives a relaunch. Loading the messages is
	 * fire-and-forget; callers don't await it.
	 */
	selectThread(threadTs: string | null): void {
		if (this.activeThreadTs === threadTs) {
			if (threadTs !== null && this.threadMessages === null) {
				void this.loadThread(threadTs);
			}
			return;
		}
		this.activeThreadTs = threadTs;
		this.threadMessages = null;
		this.threadError = null;
		// Bump the generation so any in-flight load for the previous
		// thread can't race us back into the panel.
		this.#threadGeneration += 1;
		void ipc.slack.setActiveThread(threadTs).catch(() => {});
		if (threadTs !== null) {
			void this.loadThread(threadTs);
		}
	}

	/**
	 * Pull the messages for one thread. Used both by `selectThread`
	 * and by the panel's "auto-load the persisted thread on mount"
	 * path. Safe to call concurrently — generation counter ensures
	 * only the latest result paints.
	 */
	async loadThread(threadTs: string): Promise<void> {
		const bot = this.activeBot;
		if (bot === null) {
			return;
		}
		this.loadingThread = true;
		this.threadError = null;
		const generation = ++this.#threadGeneration;
		const capturedBot = bot.user_id;
		const capturedThread = threadTs;
		try {
			const messages = await ipc.slack.getThread(bot.dm_channel_id, threadTs);
			if (
				generation !== this.#threadGeneration ||
				capturedBot !== this.activeBot?.user_id ||
				capturedThread !== this.activeThreadTs
			) {
				return;
			}
			this.threadMessages = messages;
		} catch (err) {
			if (
				generation !== this.#threadGeneration ||
				capturedBot !== this.activeBot?.user_id ||
				capturedThread !== this.activeThreadTs
			) {
				return;
			}
			this.threadMessages = [];
			this.threadError = formatError(err);
		} finally {
			if (generation === this.#threadGeneration) {
				this.loadingThread = false;
			}
		}
	}

	/**
	 * Pure read of the user-cache entry. Safe to call from `$derived`
	 * or template expressions — never mutates state, never triggers a
	 * network call. Returns `undefined` when the user has never been
	 * requested.
	 *
	 * Pair with [`requestUser`] from an `$effect`: the effect kicks
	 * off the fetch on first paint, the resulting cache write triggers
	 * a re-render, and `peekUser` then returns the resolved entry.
	 *
	 * Splitting reads from writes is what keeps Svelte's render path
	 * pure (mutating `userCache` from inside a snippet trips
	 * `state_unsafe_mutation`).
	 */
	peekUser(userId: string): SlackUserCacheEntry | undefined {
		return this.userCache.get(userId);
	}

	/**
	 * Mutating counterpart of [`peekUser`]. Idempotent — repeated calls
	 * for the same user_id are free (the cache already has an entry).
	 * Must be called *outside* render: from a Svelte `$effect`, an
	 * event handler, or after a network response.
	 */
	requestUser(userId: string): void {
		if (this.userCache.has(userId)) {
			return;
		}
		this.userCache.set(userId, { state: 'loading' });
		void this.#fetchUser(userId);
	}

	#seedSelf(): void {
		const me = this.status?.identity;
		if (me === undefined || me === null) {
			return;
		}
		this.userCache.set(me.user_id, {
			state: 'resolved',
			user: {
				user_id: me.user_id,
				name: me.user_name,
				real_name: me.user_name,
				display_name: null,
				is_bot: false,
			},
		});
	}

	#seedActiveBot(): void {
		const bot = this.activeBot;
		if (bot === null) {
			return;
		}
		this.userCache.set(bot.user_id, {
			state: 'resolved',
			user: {
				user_id: bot.user_id,
				name: bot.username,
				real_name: bot.real_name,
				display_name: bot.display_name,
				is_bot: bot.user_id !== this.status?.identity?.user_id,
			},
		});
	}

	async #fetchUser(userId: string): Promise<void> {
		try {
			const user = await ipc.slack.getUser(userId);
			this.userCache.set(userId, { state: 'resolved', user });
		} catch {
			// `users.info` failure (most often `user_not_found` for
			// deactivated members) → mark missing so the renderer falls
			// back to the raw ID and we don't retry on every paint.
			this.userCache.set(userId, { state: 'missing' });
		}
	}

	#resetUserCache(): void {
		this.userCache.clear();
	}

	#resetThreadView(): void {
		// Bump generations so any in-flight loads can't repaint.
		this.#sessionsGeneration += 1;
		this.#threadGeneration += 1;
		this.sessions = null;
		this.loadingSessions = false;
		this.sessionsError = null;
		this.activeThreadTs = null;
		this.threadMessages = null;
		this.loadingThread = false;
		this.threadError = null;
	}
}

export const slack = new SlackPanelState();
