import { describe, expect, it } from 'vitest';
import {
	groupSessions,
	isDismissable,
	isDismissed,
	sessionGroupOf,
	sessionRowState,
	withDismissal,
	type SessionRowSignals,
	type SessionRowState,
} from './sessionGroups';

function signals(overrides: Partial<SessionRowSignals> = {}): SessionRowSignals {
	return {
		awaitingInput: false,
		running: false,
		attention: false,
		lastError: false,
		interrupted: false,
		dismissed: false,
		...overrides,
	};
}

describe('sessionRowState', () => {
	it('is idle when nothing is set', () => {
		expect(sessionRowState(signals())).toBe('idle');
	});

	it('maps each lone signal to its state', () => {
		expect(sessionRowState(signals({ awaitingInput: true }))).toBe('awaiting');
		expect(sessionRowState(signals({ running: true }))).toBe('running');
		expect(sessionRowState(signals({ lastError: true }))).toBe('failed');
		expect(sessionRowState(signals({ interrupted: true }))).toBe('interrupted');
		expect(sessionRowState(signals({ attention: true }))).toBe('finished');
	});

	it('lets a parked prompt outrank the turn that is still running around it', () => {
		expect(sessionRowState(signals({ awaitingInput: true, running: true }))).toBe('awaiting');
	});

	it('lets a live turn outrank how the previous one ended', () => {
		const stale = { lastError: true, interrupted: true, attention: true };
		expect(sessionRowState(signals({ running: true, ...stale }))).toBe('running');
		expect(sessionRowState(signals({ awaitingInput: true, ...stale }))).toBe('awaiting');
	});

	it('ranks failed over interrupted over finished', () => {
		expect(sessionRowState(signals({ lastError: true, interrupted: true, attention: true }))).toBe('failed');
		expect(sessionRowState(signals({ interrupted: true, attention: true }))).toBe('interrupted');
	});
});

describe('sessionGroupOf', () => {
	it('files every state that waits on the user under needs-you', () => {
		const needsYou: SessionRowState[] = ['awaiting', 'failed', 'interrupted', 'finished'];
		for (const state of needsYou) {
			expect(sessionGroupOf(state)).toBe('needs-you');
		}
		expect(sessionGroupOf('running')).toBe('running');
		expect(sessionGroupOf('idle')).toBe('earlier');
	});
});

describe('groupSessions', () => {
	type Row = { id: string; state: SessionRowState };
	const stateOf = (row: Row) => row.state;
	const ids = (rows: Row[]) => rows.map((row) => row.id);

	it('orders the groups needs-you, running, earlier and keeps the incoming order inside each', () => {
		const rows: Row[] = [
			{ id: 'a', state: 'idle' },
			{ id: 'b', state: 'running' },
			{ id: 'c', state: 'finished' },
			{ id: 'd', state: 'idle' },
			{ id: 'e', state: 'awaiting' },
			{ id: 'f', state: 'running' },
			{ id: 'g', state: 'failed' },
			{ id: 'h', state: 'interrupted' },
		];
		const groups = groupSessions(rows, stateOf);
		expect(groups.map((group) => [group.id, group.title, ids(group.items)])).toEqual([
			['needs-you', 'Needs you', ['c', 'e', 'g', 'h']],
			['running', 'Running', ['b', 'f']],
			['earlier', 'Earlier', ['a', 'd']],
		]);
	});

	it('omits empty groups', () => {
		const groups = groupSessions<Row>(
			[
				{ id: 'a', state: 'idle' },
				{ id: 'b', state: 'idle' },
			],
			stateOf,
		);
		expect(groups.map((group) => group.id)).toEqual(['earlier']);
		expect(groupSessions<Row>([], stateOf)).toEqual([]);
	});
});

describe('dismissal', () => {
	it('sets aside how the last turn ended, not a live turn', () => {
		const ended = { lastError: true, interrupted: true, attention: true, dismissed: true };
		expect(sessionRowState(signals(ended))).toBe('idle');
		expect(sessionRowState(signals({ ...ended, running: true }))).toBe('running');
		expect(sessionRowState(signals({ ...ended, awaitingInput: true }))).toBe('awaiting');
	});

	it('is offered for ended states only', () => {
		expect((['failed', 'interrupted', 'finished'] as const).every(isDismissable)).toBe(true);
		expect((['awaiting', 'running', 'idle'] as const).some(isDismissable)).toBe(false);
	});

	it('holds until the session is written to again', () => {
		const summary = { id: 'a', updated_at_ms: 100, title: 'not kept' };
		const dismissals = withDismissal([], summary);
		expect(dismissals).toEqual([{ id: 'a', updated_at_ms: 100 }]);
		expect(isDismissed(dismissals, { id: 'a', updated_at_ms: 100 })).toBe(true);
		expect(isDismissed(dismissals, { id: 'a', updated_at_ms: 101 })).toBe(false);
		expect(isDismissed(dismissals, { id: 'b', updated_at_ms: 100 })).toBe(false);
	});

	it('replaces an older dismissal of the same session', () => {
		const dismissals = withDismissal([{ id: 'a', updated_at_ms: 100 }], { id: 'a', updated_at_ms: 200 });
		expect(dismissals).toEqual([{ id: 'a', updated_at_ms: 200 }]);
	});
});
