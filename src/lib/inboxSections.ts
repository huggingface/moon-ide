// Pure inbox logic (ADR 0112): why an item is listed, which items the
// filter chips let through, how a partial poll combines with the
// previous one, and the text filter. Kept apart from the store so it
// can be tested without runes.

import type { InboxItem, InboxSnapshot, InboxSnooze, InboxSource } from './protocol';

/** A reason an item is in the inbox. An item can have several. */
export type InboxCategory = 'unread' | 'review' | 'watched' | 'mentioned' | 'mine';

/** A filter chip: a category, or the snoozed items. */
export type InboxChip = InboxCategory | 'snoozed';

/** Chip order, which is also the order of a row's reason tags: what
 * needs an answer first. */
export const INBOX_CATEGORIES: readonly InboxCategory[] = ['unread', 'review', 'watched', 'mentioned', 'mine'];

export const CHIP_LABELS: Record<InboxChip, string> = {
	unread: 'Unread',
	review: 'Review requested',
	watched: 'Watched',
	mentioned: 'Mentioned',
	mine: 'Mine',
	snoozed: 'Snoozed',
};

export type InboxRow = {
	item: InboxItem;
	categories: InboxCategory[];
	snoozed: boolean;
};

export type InboxList = {
	/** Each listed item once, newest first. */
	items: InboxRow[];
	/** Items per chip that match the text filter, whichever chips are
	 * active. A snoozed item counts under `snoozed` only. */
	counts: Record<InboxChip, number>;
};

function isOpenPr(item: InboxItem): boolean {
	return item.kind === 'pull_request' && (item.state === 'open' || item.state === 'draft');
}

function inCategory(item: InboxItem, category: InboxCategory, viewer: string): boolean {
	if (category === 'unread') {
		return item.notification !== null;
	}
	if (category === 'watched') {
		return item.sources.includes('watched');
	}
	if (category === 'mentioned') {
		return item.sources.includes('mentioned');
	}
	if (category === 'mine') {
		return item.sources.includes('mine');
	}
	// A team request a teammate already answered drops out of the
	// `review-requested:@me` search while the thread still says the
	// user was asked; the unread reason keeps it listed.
	return (
		item.sources.includes('review_requested') ||
		(item.notification?.reason === 'review_requested' && isOpenPr(item) && item.author !== viewer)
	);
}

export function categoriesOf(item: InboxItem, viewer: string): InboxCategory[] {
	return INBOX_CATEGORIES.filter((category) => inCategory(item, category, viewer));
}

/** Category a notification reason already names. */
const REASON_CATEGORY: Record<string, InboxCategory> = {
	review_requested: 'review',
	mention: 'mentioned',
	author: 'mine',
};

/** A category a row can carry as a quiet tag. Unread is not one: an
 * unread row shows its notification reason instead. */
export type InboxTag = Exclude<InboxCategory, 'unread'>;

/** The chip names, cut to a word so a row has room for its title. */
export const TAG_LABELS: Record<InboxTag, string> = {
	review: 'review',
	watched: 'watched',
	mentioned: 'mentioned',
	mine: 'mine',
};

/** Tags a row shows next to the unread reason: its other categories,
 * minus the one that reason already states. */
export function quietTags(row: InboxRow): InboxTag[] {
	const stated = row.item.notification === null ? undefined : REASON_CATEGORY[row.item.notification.reason];
	return row.categories.filter((category): category is InboxTag => category !== 'unread' && category !== stated);
}

/** An unread item is as new as its notification. */
function sortKey(item: InboxItem): string {
	return item.notification?.updatedAt ?? item.updatedAt;
}

export function matchesFilter(item: InboxItem, filter: string): boolean {
	const needle = filter.trim().toLowerCase();
	if (needle === '') {
		return true;
	}
	const haystack = [
		item.title,
		item.repo,
		item.author ?? '',
		item.headRef ?? '',
		item.number === null ? '' : `#${item.number}`,
	]
		.join('\n')
		.toLowerCase();
	return needle.split(/\s+/).every((word) => haystack.includes(word));
}

/** The single inbox list. With no chip active it is every item that
 * isn't snoozed; with some active, the items in any of those
 * categories, plus the snoozed ones when that chip is among them. */
export function buildList(
	snapshot: InboxSnapshot | null,
	filter: string,
	snoozed: ReadonlySet<string>,
	active: ReadonlySet<InboxChip>,
): InboxList {
	const counts: Record<InboxChip, number> = { unread: 0, review: 0, watched: 0, mentioned: 0, mine: 0, snoozed: 0 };
	if (snapshot === null) {
		return { items: [], counts };
	}
	const items: InboxRow[] = [];
	for (const item of snapshot.items) {
		if (!matchesFilter(item, filter)) {
			continue;
		}
		const row: InboxRow = {
			item,
			categories: categoriesOf(item, snapshot.viewer),
			snoozed: snoozed.has(item.id),
		};
		if (row.snoozed) {
			counts.snoozed += 1;
			if (active.has('snoozed')) {
				items.push(row);
			}
			continue;
		}
		for (const category of row.categories) {
			counts[category] += 1;
		}
		if (active.size === 0 || row.categories.some((category) => active.has(category))) {
			items.push(row);
		}
	}
	items.sort((a, b) => sortKey(b.item).localeCompare(sortKey(a.item)));
	return { items, counts };
}

/** Snoozes still in force at `now`. */
export function activeSnoozes(snoozes: readonly InboxSnooze[], now: number): InboxSnooze[] {
	return snoozes.filter((snooze) => Date.parse(snooze.until) > now);
}

/** When a snooze ends: 08:00 local the next day, or 08:00 local the
 * next Monday (a week on, when today is Monday). */
export function snoozeUntil(span: 'tomorrow' | 'next_week', now: Date): Date {
	const until = new Date(now);
	const days = span === 'tomorrow' ? 1 : (8 - now.getDay()) % 7 || 7;
	until.setDate(until.getDate() + days);
	until.setHours(8, 0, 0, 0);
	return until;
}

/** "tomorrow 08:00" / "Mon 08:00" for a snooze's end. */
export function snoozeLabel(until: string, now: Date): string {
	const end = new Date(until);
	const time = `${String(end.getHours()).padStart(2, '0')}:${String(end.getMinutes()).padStart(2, '0')}`;
	const startOfDay = (date: Date) => new Date(date.getFullYear(), date.getMonth(), date.getDate()).getTime();
	const days = Math.round((startOfDay(end) - startOfDay(now)) / 86_400_000);
	if (days <= 0) {
		return `today ${time}`;
	}
	if (days === 1) {
		return `tomorrow ${time}`;
	}
	return `${['Sun', 'Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat'][end.getDay()]} ${time}`;
}

/** Combine a poll with the one before it. A source that failed this
 * time keeps the rows it had, so one slow search doesn't drop them
 * until the next poll. */
export function mergeSnapshot(previous: InboxSnapshot | null, next: InboxSnapshot): InboxSnapshot {
	if (previous === null || next.status.kind !== 'ok' || next.failures.length === 0) {
		return next;
	}
	const failed = new Set<InboxSource>(next.failures.map((failure) => failure.source));
	const items = next.items.map((item) => ({ ...item, sources: [...item.sources] }));
	const byId = new Map(items.map((item) => [item.id, item]));
	for (const old of previous.items) {
		const carried = old.sources.filter((source) => failed.has(source));
		if (carried.length === 0) {
			continue;
		}
		const current = byId.get(old.id);
		if (current === undefined) {
			items.push({
				...old,
				sources: carried,
				notification: failed.has('notification') ? old.notification : null,
			});
			continue;
		}
		for (const source of carried) {
			if (!current.sources.includes(source)) {
				current.sources.push(source);
			}
		}
		if (failed.has('notification') && current.notification === null) {
			current.notification = old.notification;
		}
	}
	return { ...next, viewer: next.viewer || previous.viewer, items };
}

/** The snapshot with one thread marked read: the item loses its
 * notification, and leaves the list when nothing else put it there. */
export function withoutNotification(snapshot: InboxSnapshot, itemId: string): InboxSnapshot {
	const items: InboxItem[] = [];
	for (const item of snapshot.items) {
		if (item.id !== itemId) {
			items.push(item);
			continue;
		}
		const sources = item.sources.filter((source) => source !== 'notification');
		if (sources.length > 0) {
			items.push({ ...item, sources, notification: null });
		}
	}
	return { ...snapshot, items };
}

/** `owner/name` → `name`; the owner is noise in a list that is mostly
 * one organisation. */
export function shortRepo(repo: string): string {
	const slash = repo.indexOf('/');
	return slash === -1 ? repo : repo.slice(slash + 1);
}

const REASON_LABELS: Record<string, string> = {
	review_requested: 'review requested',
	mention: 'mentioned',
	team_mention: 'team mentioned',
	author: 'your PR',
	comment: 'commented',
	assign: 'assigned',
	state_change: 'state changed',
	ci_activity: 'CI',
	subscribed: 'watching',
	manual: 'subscribed',
	approval_requested: 'approval requested',
};

export function reasonLabel(reason: string): string {
	return REASON_LABELS[reason] ?? reason.replaceAll('_', ' ');
}

export function relativeTime(iso: string, now: number): string {
	const then = Date.parse(iso);
	if (Number.isNaN(then)) {
		return '';
	}
	const minutes = Math.max(0, Math.round((now - then) / 60_000));
	if (minutes < 1) {
		return 'now';
	}
	if (minutes < 60) {
		return `${minutes}m`;
	}
	const hours = Math.round(minutes / 60);
	if (hours < 24) {
		return `${hours}h`;
	}
	const days = Math.round(hours / 24);
	if (days < 30) {
		return `${days}d`;
	}
	return `${Math.round(days / 30)}mo`;
}
