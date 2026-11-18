import { describe, expect, it } from 'vitest';
import { mergeOffer } from './prActions';
import type { PrCheckState, PullRequestDetail } from './protocol';

type Subject = Parameters<typeof mergeOffer>[0];

function pr(over: Partial<Subject> = {}): Subject {
	return { state: 'open', mergeStatus: 'clean', review: null, checks: [], baseRef: 'main', headRef: 'fix', ...over };
}

function checks(...states: PrCheckState[]): PullRequestDetail['checks'] {
	return states.map((state, index) => ({ name: `check ${index}`, state, url: null }));
}

describe('mergeOffer', () => {
	it('offers the merge when GitHub would take it', () => {
		expect(mergeOffer(pr())).toEqual({ kind: 'merge', caution: null });
	});

	it('asks a draft to be marked ready, whatever its merge status', () => {
		expect(mergeOffer(pr({ state: 'draft', mergeStatus: 'blocked' }))).toEqual({ kind: 'ready' });
	});

	it('offers nothing for a merged or closed pull request', () => {
		expect(mergeOffer(pr({ state: 'merged' }))).toEqual({ kind: 'none' });
		expect(mergeOffer(pr({ state: 'closed' }))).toEqual({ kind: 'none' });
	});

	it('still merges past optional checks, and says which way they went', () => {
		expect(mergeOffer(pr({ mergeStatus: 'unstable', checks: checks('success', 'failure') }))).toEqual({
			kind: 'merge',
			caution: 'Some checks are failing',
		});
		expect(mergeOffer(pr({ mergeStatus: 'unstable', checks: checks('pending') }))).toEqual({
			kind: 'merge',
			caution: 'Some checks are still running',
		});
	});

	it('lets an unknown status through with a caution', () => {
		expect(mergeOffer(pr({ mergeStatus: 'unknown' })).kind).toBe('merge');
	});

	it('names what blocks the merge, the review first', () => {
		const blocked = (over: Partial<Subject>) => mergeOffer(pr({ mergeStatus: 'blocked', ...over }));
		expect(blocked({ review: 'changes_requested', checks: checks('failure') })).toEqual({
			kind: 'blocked',
			reason: 'Changes were requested',
		});
		expect(blocked({ review: 'review_required' })).toEqual({ kind: 'blocked', reason: 'Needs an approving review' });
		expect(blocked({ review: 'approved', checks: checks('failure') })).toEqual({
			kind: 'blocked',
			reason: 'A required check is failing',
		});
		expect(blocked({ review: 'approved', checks: checks('pending') })).toEqual({
			kind: 'blocked',
			reason: 'Required checks are still running',
		});
		expect(blocked({ review: 'approved' })).toEqual({ kind: 'blocked', reason: 'The rules of main block the merge' });
	});

	it('names a stale or conflicting branch', () => {
		expect(mergeOffer(pr({ mergeStatus: 'behind' }))).toEqual({ kind: 'blocked', reason: 'fix is behind main' });
		expect(mergeOffer(pr({ mergeStatus: 'conflicting' }))).toEqual({ kind: 'blocked', reason: 'Conflicts with main' });
	});
});
