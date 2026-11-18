//! GitHub inbox store (ADR 0112): the last poll, the list the filter
//! chips select from it, and the polling schedule.
//!
//! The store never touches the workspace. Opening a pull request needs
//! `WorkspaceState`, which already imports this module for hydration,
//! so that gesture lives in `InboxView`.

import { ipc } from './ipc';
import { SvelteSet } from 'svelte/reactivity';
import {
	activeSnoozes,
	applyGrouping,
	buildList,
	isOpenPr,
	mergeSnapshot,
	pruneGrouping,
	withoutNotification,
	type InboxChip,
} from './inboxSections';
import {
	formatError,
	type InboxGroup,
	type InboxGroupAssignment,
	type InboxItem,
	type InboxPrDigest,
	type InboxSnapshot,
	type InboxSnooze,
	type InboxState,
} from './protocol';

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
	/** The inbox tab is open. Owned by the workspace store, which
	 * keeps the tab pinned in every folder; kept here to be saved
	 * with the rest of the inbox's state. */
	tabOpen = $state(false);

	/** Notes, review threads and changed paths per pull request
	 * (ADR 0119), with the `updatedAt` each was read at: a row is
	 * read again only once GitHub says it changed. */
	digests = $state<Record<string, { updatedAt: string; digest: InboxPrDigest }>>({});

	/** The list is shown by group. */
	groupByTopic = $state(false);
	/** Groups the model made, and where it put each pull request. A
	 * pull request stays in its group until it leaves the inbox or
	 * the user asks for a regroup. */
	groups = $state<InboxGroup[]>([]);
	grouped = $state<InboxGroupAssignment[]>([]);
	/** A grouping call is running. */
	grouping = $state(false);
	groupError = $state<string | null>(null);
	readonly groupOf = $derived(new Map(this.grouped.map((assignment) => [assignment.id, assignment.group])));

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
	#readingDigests = false;
	/** Pull requests a grouping call was asked about and didn't place.
	 * They aren't offered again by themselves, or a model that keeps
	 * leaving one out would be asked at every poll. */
	readonly #unplaced = new Set<string>();

	hydrate(state: InboxState): void {
		this.watchedAuthors = [...state.watched_authors];
		this.snoozed = activeSnoozes(state.snoozed, Date.now());
		this.tabOpen = state.tab_open;
		this.groupByTopic = state.group_by_topic;
		this.groups = [...state.groups];
		this.grouped = [...state.grouped];
	}

	serialise(): InboxState {
		return {
			watched_authors: this.watchedAuthors,
			snoozed: this.snoozed,
			tab_open: this.tabOpen,
			group_by_topic: this.groupByTopic,
			groups: this.groups,
			grouped: this.grouped,
		};
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
		await this.#readDigests();
		this.#forgetGone();
		void this.#placeNew();
	}

	/** Read the digest of every open pull request that is new or has
	 * changed since its last one. Best-effort: a row without one just
	 * shows no counts, and is asked for again at the next poll. */
	async #readDigests(): Promise<void> {
		const items = this.snapshot?.items ?? [];
		const stale = items.filter(
			(item) => isOpenPr(item) && item.number !== null && this.digests[item.id]?.updatedAt !== item.updatedAt,
		);
		if (stale.length === 0 || this.#readingDigests) {
			return;
		}
		this.#readingDigests = true;
		try {
			const read = await ipc.inbox.digests(stale.map((item) => ({ repo: item.repo, number: item.number ?? 0 })));
			const updatedAt = new Map(stale.map((item) => [item.id, item.updatedAt]));
			const live = new Set((this.snapshot?.items ?? items).map((item) => item.id));
			const next = Object.fromEntries(Object.entries(this.digests).filter(([id]) => live.has(id)));
			for (const digest of read) {
				next[digest.id] = { updatedAt: updatedAt.get(digest.id) ?? '', digest };
			}
			this.digests = next;
		} catch {
			// The rows stay as they were.
		} finally {
			this.#readingDigests = false;
		}
	}

	/** Drop what is kept about pull requests that left the inbox. Only
	 * after a complete poll: a source that failed says nothing about
	 * whether its items are gone. */
	#forgetGone(): void {
		const snapshot = this.snapshot;
		if (snapshot === null || snapshot.status.kind !== 'ok' || snapshot.failures.length > 0) {
			return;
		}
		const pruned = pruneGrouping(this.groups, this.grouped, new Set(snapshot.items.map((item) => item.id)));
		if (pruned.grouped.length === this.grouped.length && pruned.groups.length === this.groups.length) {
			return;
		}
		this.groups = pruned.groups;
		this.grouped = pruned.grouped;
		this.#onChange?.();
	}

	setGroupByTopic(on: boolean): void {
		if (this.groupByTopic === on) {
			return;
		}
		this.groupByTopic = on;
		this.#onChange?.();
		void this.#placeNew(true);
	}

	/** Sort every pull request afresh. The groups on screen stay until
	 * the new ones arrive, so a failed call loses nothing. */
	regroup(): void {
		void this.#placeNew(true, true);
	}

	/** Try the grouping call again after a failure. */
	retryGrouping(): void {
		void this.#placeNew(true);
	}

	/** Ask the model where the pull requests without a group go
	 * (ADR 0119). Runs only while the list is shown by group, and
	 * only for pull requests it hasn't been asked about. After a
	 * failure nothing is asked again until the user says so
	 * (`retry`): a poll every minute is no reason to repeat a call
	 * that isn't working. `afresh` places every pull request again
	 * and replaces the groups with the answer. */
	async #placeNew(retry = false, afresh = false): Promise<void> {
		if (!this.groupByTopic || this.grouping || (this.groupError !== null && !retry)) {
			return;
		}
		const pending = (this.snapshot?.items ?? []).filter(
			(item) => isOpenPr(item) && (afresh || (!this.groupOf.has(item.id) && !this.#unplaced.has(item.id))),
		);
		if (pending.length === 0) {
			return;
		}
		this.grouping = true;
		try {
			const answer = await ipc.inbox.group({
				items: pending.map((item) => ({
					id: item.id,
					repo: item.repo,
					title: item.title,
					author: item.author,
					headRef: item.headRef,
					additions: item.additions,
					deletions: item.deletions,
					paths: this.digests[item.id]?.digest.paths ?? [],
					summary: this.digests[item.id]?.digest.summary ?? '',
				})),
				existing: afresh ? [] : this.groups,
			});
			const merged = afresh ? applyGrouping([], [], answer) : applyGrouping(this.groups, this.grouped, answer);
			this.groups = merged.groups;
			this.grouped = merged.grouped;
			if (afresh) {
				this.#unplaced.clear();
			}
			for (const item of pending) {
				if (!this.groupOf.has(item.id)) {
					this.#unplaced.add(item.id);
				}
			}
			this.groupError = null;
			this.#onChange?.();
		} catch (err) {
			this.groupError = formatError(err);
		} finally {
			this.grouping = false;
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
