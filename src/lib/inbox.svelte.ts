//! GitHub inbox store (ADR 0112): the last poll, the list the filter
//! chips select from it, and the polling schedule.
//!
//! The store never touches the workspace. Opening a pull request needs
//! `WorkspaceState`, which already imports this module for hydration,
//! so that gesture lives in `InboxView`.

import { ipc } from './ipc';
import { SvelteSet } from 'svelte/reactivity';
import { activeSnoozes, buildList, mergeSnapshot, withoutNotification, type InboxChip } from './inboxSections';
import { formatError, type InboxState, type InboxItem, type InboxSnapshot, type InboxSnooze } from './protocol';

/** Poll period while an inbox tab is on screen in a focused window. */
const VISIBLE_POLL_MS = 75_000;
/** Poll period otherwise; keeps the status-bar count honest. */
const BACKGROUND_POLL_MS = 300_000;
const TICK_MS = 15_000;

function sameAuthors(a: readonly string[], b: readonly string[]): boolean {
	return a.length === b.length && a.every((author, index) => author === b[index]);
}

class InboxStore {
	snapshot = $state<InboxSnapshot | null>(null);
	loading = $state(false);
	/** Set when the last poll never reached `gh` (the call itself
	 * failed). Per-source failures are on the snapshot instead. */
	error = $state<string | null>(null);
	fetchedAt = $state<number | null>(null);
	watchedAuthors = $state<string[]>([]);
	/** Items hidden until a time. Local to this workspace: GitHub has
	 * no such state, and a snoozed thread stays unread there. */
	snoozed = $state<InboxSnooze[]>([]);
	filter = $state('');

	/** Filter chips switched on in the tab. None means every item
	 * that isn't snoozed. Not persisted: a fresh window starts with
	 * the whole list. */
	readonly activeChips = new SvelteSet<InboxChip>();

	readonly snoozedIds = $derived(new Set(this.snoozed.map((snooze) => snooze.id)));
	/** What the tab shows: the rows the chips and the text filter let
	 * through, and each chip's count under that text filter. */
	readonly list = $derived(buildList(this.snapshot, this.filter, this.snoozedIds, this.activeChips));
	/** Items per category, ignoring the text filter and the chips:
	 * what the sidebar button's badges show. */
	readonly counts = $derived(buildList(this.snapshot, '', this.snoozedIds, new Set()).counts);

	#viewers = 0;
	#timer: number | null = null;
	#lastAttempt = 0;
	#onChange: (() => void) | null = null;

	hydrate(state: InboxState): void {
		this.watchedAuthors = [...state.watched_authors];
		this.snoozed = activeSnoozes(state.snoozed, Date.now());
	}

	serialise(): InboxState {
		return { watched_authors: this.watchedAuthors, snoozed: this.snoozed };
	}

	/** Called whenever the persisted part (authors, snoozes) changes. */
	bindOnChange(handler: () => void): void {
		this.#onChange = handler;
	}

	toggleChip(chip: InboxChip): void {
		if (!this.activeChips.delete(chip)) {
			this.activeChips.add(chip);
		}
	}

	clearChips(): void {
		this.activeChips.clear();
	}

	/** Hide `item` until `until`. Snoozing again moves the time. */
	snooze(item: InboxItem, until: Date): void {
		this.snoozed = [
			...this.snoozed.filter((snooze) => snooze.id !== item.id),
			{ id: item.id, until: until.toISOString() },
		];
		this.#onChange?.();
	}

	unsnooze(id: string): void {
		if (!this.snoozedIds.has(id)) {
			return;
		}
		this.snoozed = this.snoozed.filter((snooze) => snooze.id !== id);
		this.#onChange?.();
	}

	snoozedUntil(id: string): string | null {
		return this.snoozed.find((snooze) => snooze.id === id)?.until ?? null;
	}

	#wakeExpired(): void {
		const active = activeSnoozes(this.snoozed, Date.now());
		if (active.length !== this.snoozed.length) {
			this.snoozed = active;
			this.#onChange?.();
		}
	}

	setWatchedAuthors(authors: readonly string[]): void {
		const next = [...new Set(authors.map((author) => author.trim().replace(/^@/, '')).filter((a) => a !== ''))];
		if (sameAuthors(next, this.watchedAuthors)) {
			return;
		}
		this.watchedAuthors = next;
		this.#onChange?.();
		void this.refresh();
	}

	/** Begin polling. Idempotent. */
	start(): void {
		if (this.#timer !== null || typeof window === 'undefined') {
			return;
		}
		void this.refresh();
		this.#timer = window.setInterval(() => this.#tick(), TICK_MS);
		window.addEventListener('focus', () => this.#tick());
	}

	/** Register an on-screen inbox tab; returns its unregister. */
	attachView(): () => void {
		this.#viewers += 1;
		this.#tick();
		return () => {
			this.#viewers -= 1;
		};
	}

	#tick(): void {
		this.#wakeExpired();
		const watching = this.#viewers > 0 && document.hasFocus();
		// Without a usable `gh` there is nothing to keep fresh in the
		// background; an open tab still retries so a fix shows up.
		if (!watching && this.snapshot !== null && this.snapshot.status.kind !== 'ok') {
			return;
		}
		const period = watching ? VISIBLE_POLL_MS : BACKGROUND_POLL_MS;
		if (Date.now() - this.#lastAttempt >= period) {
			void this.refresh();
		}
	}

	async refresh(): Promise<void> {
		if (this.loading) {
			return;
		}
		this.loading = true;
		this.#lastAttempt = Date.now();
		try {
			const next = await ipc.inbox.fetch(this.watchedAuthors);
			this.snapshot = mergeSnapshot(this.snapshot, next);
			this.fetchedAt = Date.now();
			this.error = null;
		} catch (err) {
			this.error = formatError(err);
		} finally {
			this.loading = false;
		}
	}

	/** Mark an item's notification thread read. The row updates at
	 * once; a failure puts the truth back with a fresh poll. */
	async markRead(item: InboxItem): Promise<void> {
		const thread = item.notification;
		if (thread === null || this.snapshot === null) {
			return;
		}
		this.snapshot = withoutNotification(this.snapshot, item.id);
		try {
			await ipc.inbox.markRead(thread.threadId);
		} catch (err) {
			this.error = formatError(err);
			void this.refresh();
		}
	}

	/** Folder bound to `repo`, when the workspace has one. */
	folderFor(repo: string): string | null {
		const wanted = repo.toLowerCase();
		return this.snapshot?.repos.find((bound) => bound.repo.toLowerCase() === wanted)?.folderPath ?? null;
	}
}

export const inbox = new InboxStore();
