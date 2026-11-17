import { describe, expect, it } from 'vitest';
import {
	activeSnoozes,
	buildList,
	categoriesOf,
	matchesFilter,
	mergeSnapshot,
	quietTags,
	relativeTime,
	snoozeLabel,
	snoozeUntil,
	withoutNotification,
	type InboxChip,
} from './inboxSections';
import type { InboxItem, InboxSnapshot } from './protocol';

function item(overrides: Partial<InboxItem>): InboxItem {
	return {
		id: 'acme/app#1',
		kind: 'pull_request',
		repo: 'acme/app',
		number: 1,
		title: 'Fix the thing',
		url: 'https://github.com/acme/app/pull/1',
		state: 'open',
		author: 'octocat',
		authorAvatarUrl: null,
		updatedAt: '2026-11-17T10:00:00Z',
		headRef: 'fix-thing',
		baseRef: 'main',
		isCrossRepository: false,
		additions: 1,
		deletions: 1,
		commentCount: 0,
		review: null,
		ci: null,
		sources: ['mine'],
		notification: null,
		...overrides,
	};
}

function snapshot(items: InboxItem[], overrides: Partial<InboxSnapshot> = {}): InboxSnapshot {
	return { status: { kind: 'ok' }, viewer: 'me', items, repos: [], failures: [], ...overrides };
}

const thread = { threadId: '9', reason: 'review_requested', updatedAt: '2026-11-17T12:00:00Z' };

/** review + unread, mine, watched + mentioned, and an unread issue. */
function mixed(): InboxSnapshot {
	return snapshot([
		item({ id: 'acme/app#1', sources: ['review_requested', 'notification'], notification: thread }),
		item({ id: 'acme/app#2', number: 2, updatedAt: '2026-11-17T09:00:00Z' }),
		item({ id: 'acme/app#3', number: 3, sources: ['watched', 'mentioned'], updatedAt: '2026-11-17T11:00:00Z' }),
		item({
			id: 'acme/app#4',
			number: 4,
			kind: 'issue',
			state: null,
			sources: ['notification'],
			notification: { threadId: '10', reason: 'subscribed', updatedAt: '2026-11-17T08:00:00Z' },
		}),
	]);
}

function list(source: InboxSnapshot, chips: InboxChip[] = [], snoozed: string[] = [], filter = '') {
	const built = buildList(source, filter, new Set(snoozed), new Set(chips));
	return { numbers: built.items.map((row) => row.item.number), counts: built.counts, rows: built.items };
}

describe('buildList', () => {
	it('lists every item once, newest first, when no chip is active', () => {
		const { numbers, rows } = list(mixed());
		// #1 sorts by its notification (12:00), not its own 10:00.
		expect(numbers).toEqual([1, 3, 2, 4]);
		expect(rows.map((row) => row.categories)).toEqual([
			['unread', 'review'],
			['watched', 'mentioned'],
			['mine'],
			['unread'],
		]);
	});

	it('shows the union of the active chips', () => {
		expect(list(mixed(), ['unread']).numbers).toEqual([1, 4]);
		expect(list(mixed(), ['mine']).numbers).toEqual([2]);
		expect(list(mixed(), ['review', 'mentioned']).numbers).toEqual([1, 3]);
		expect(list(mixed(), ['unread', 'review', 'watched', 'mentioned', 'mine']).numbers).toEqual([1, 3, 2, 4]);
	});

	it('counts each category whichever chips are active', () => {
		const expected = { unread: 2, review: 1, watched: 1, mentioned: 1, mine: 1, snoozed: 0 };
		expect(list(mixed()).counts).toEqual(expected);
		expect(list(mixed(), ['mine']).counts).toEqual(expected);
	});

	it('counts and lists under the text filter', () => {
		const { numbers, counts } = list(mixed(), [], [], '#3');
		expect(numbers).toEqual([3]);
		expect(counts).toEqual({ unread: 0, review: 0, watched: 1, mentioned: 1, mine: 0, snoozed: 0 });
	});

	it('keeps snoozed items out unless their chip is active', () => {
		const snoozed = ['acme/app#1'];
		const hidden = list(mixed(), [], snoozed);
		expect(hidden.numbers).toEqual([3, 2, 4]);
		expect(hidden.counts).toEqual({ unread: 1, review: 0, watched: 1, mentioned: 1, mine: 1, snoozed: 1 });
		// Its own categories do not bring a snoozed item back.
		expect(list(mixed(), ['review'], snoozed).numbers).toEqual([]);

		const alone = list(mixed(), ['snoozed'], snoozed);
		expect(alone.numbers).toEqual([1]);
		expect(alone.rows[0]?.snoozed).toBe(true);
		expect(list(mixed(), ['snoozed', 'mine'], snoozed).numbers).toEqual([1, 2]);
	});

	it('is empty without a snapshot', () => {
		expect(buildList(null, '', new Set(), new Set()).items).toEqual([]);
	});
});

describe('categoriesOf', () => {
	it('keeps a team review request alive through its unread thread', () => {
		const answered = item({ sources: ['notification'], notification: thread });
		expect(categoriesOf(answered, 'me')).toEqual(['unread', 'review']);
		const own = item({ sources: ['notification'], notification: thread, author: 'me' });
		expect(categoriesOf(own, 'me')).toEqual(['unread']);
	});
});

describe('quietTags', () => {
	it('tags a row with its categories, without repeating the unread reason', () => {
		const tags = (chips: InboxChip[] = []) => list(mixed(), chips).rows.map((row) => quietTags(row));
		// #1 is unread because a review was requested: the reason pill says so.
		expect(tags()).toEqual([[], ['watched', 'mentioned'], ['mine'], []]);
		const mentionedInOwn = item({
			sources: ['mine', 'watched', 'notification'],
			notification: { threadId: '11', reason: 'mention', updatedAt: '2026-11-17T12:00:00Z' },
		});
		const [row] = buildList(snapshot([mentionedInOwn]), '', new Set(), new Set()).items;
		expect(row && quietTags(row)).toEqual(['watched', 'mine']);
	});
});

describe('matchesFilter', () => {
	it('matches every word against title, repo, author, branch and number', () => {
		const pr = item({});
		expect(matchesFilter(pr, '')).toBe(true);
		expect(matchesFilter(pr, 'octocat app')).toBe(true);
		expect(matchesFilter(pr, '#1')).toBe(true);
		expect(matchesFilter(pr, 'fix-thing')).toBe(true);
		expect(matchesFilter(pr, 'octocat nope')).toBe(false);
	});
});

describe('mergeSnapshot', () => {
	it('carries the rows of a failed source over from the previous poll', () => {
		const previous = snapshot([
			item({ id: 'acme/app#1', sources: ['mine', 'watched'] }),
			item({ id: 'acme/app#2', number: 2, sources: ['watched'] }),
			item({ id: 'acme/app#3', number: 3, sources: ['mentioned'] }),
		]);
		const next = snapshot([item({ id: 'acme/app#1', sources: ['mine'] })], {
			failures: [{ source: 'watched', detail: 'HTTP 502' }],
		});
		const merged = mergeSnapshot(previous, next);
		expect(merged.items.map((i) => [i.id, i.sources])).toEqual([
			['acme/app#1', ['mine', 'watched']],
			['acme/app#2', ['watched']],
		]);
	});

	it('takes a clean poll as is', () => {
		const next = snapshot([item({})]);
		expect(mergeSnapshot(snapshot([item({ id: 'acme/app#9' })]), next)).toBe(next);
	});
});

describe('withoutNotification', () => {
	it('drops the thread, and the item when nothing else listed it', () => {
		const listed = item({ id: 'acme/app#1', sources: ['mine', 'notification'], notification: thread });
		const only = item({ id: 'acme/app#2', sources: ['notification'], notification: thread });
		const after = withoutNotification(withoutNotification(snapshot([listed, only]), 'acme/app#1'), 'acme/app#2');
		expect(after.items).toEqual([{ ...listed, sources: ['mine'], notification: null }]);
	});
});

describe('relativeTime', () => {
	it('buckets by minute, hour, day and month', () => {
		const now = Date.parse('2026-11-17T12:00:00Z');
		expect(relativeTime('2026-11-17T11:59:50Z', now)).toBe('now');
		expect(relativeTime('2026-11-17T11:15:00Z', now)).toBe('45m');
		expect(relativeTime('2026-11-17T07:00:00Z', now)).toBe('5h');
		expect(relativeTime('2026-11-14T12:00:00Z', now)).toBe('3d');
		expect(relativeTime('2026-08-17T12:00:00Z', now)).toBe('3mo');
		expect(relativeTime('garbage', now)).toBe('');
	});
});

describe('snooze', () => {
	it('ends at 08:00 the next day, or the next Monday', () => {
		const wednesday = new Date(2026, 10, 18, 15, 30);
		expect(snoozeUntil('tomorrow', wednesday)).toEqual(new Date(2026, 10, 19, 8, 0));
		expect(snoozeUntil('next_week', wednesday)).toEqual(new Date(2026, 10, 23, 8, 0));
		const monday = new Date(2026, 10, 16, 7, 0);
		expect(snoozeUntil('next_week', monday)).toEqual(new Date(2026, 10, 23, 8, 0));
		const sunday = new Date(2026, 10, 22, 23, 0);
		expect(snoozeUntil('next_week', sunday)).toEqual(new Date(2026, 10, 23, 8, 0));
	});

	it('drops snoozes whose time has passed', () => {
		const now = Date.parse('2026-11-18T12:00:00Z');
		const snoozes = [
			{ id: 'a', until: '2026-11-18T11:59:00Z' },
			{ id: 'b', until: '2026-11-19T07:00:00Z' },
		];
		expect(activeSnoozes(snoozes, now).map((s) => s.id)).toEqual(['b']);
	});

	it('labels the end relative to today', () => {
		const now = new Date(2026, 10, 18, 15, 30);
		expect(snoozeLabel(new Date(2026, 10, 19, 8, 0).toISOString(), now)).toBe('tomorrow 08:00');
		expect(snoozeLabel(new Date(2026, 10, 23, 8, 0).toISOString(), now)).toBe('Mon 08:00');
		expect(snoozeLabel(new Date(2026, 10, 18, 18, 5).toISOString(), now)).toBe('today 18:05');
	});
});
