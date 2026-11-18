// What the pull request panel offers to do with a pull request's
// state (ADR 0120), worked out from what GitHub reports about it.

import type { PrMergeMethod, PullRequestDetail } from './protocol';

export const MERGE_LABELS: Record<PrMergeMethod, string> = {
	squash: 'Squash and merge',
	merge: 'Merge',
	rebase: 'Rebase and merge',
};

/** What stands where the merge button goes. */
export type MergeOffer =
	/** Merged or closed: nothing to merge. */
	| { kind: 'none' }
	/** A draft: it has to be marked ready first. */
	| { kind: 'ready' }
	/** GitHub would take it. `caution` is something to know first. */
	| { kind: 'merge'; caution: string | null }
	/** GitHub would refuse, for `reason`. */
	| { kind: 'blocked'; reason: string };

type Subject = Pick<PullRequestDetail, 'state' | 'mergeStatus' | 'review' | 'checks' | 'baseRef' | 'headRef'>;

function blockedReason(pr: Subject): string {
	if (pr.review === 'changes_requested') {
		return 'Changes were requested';
	}
	if (pr.review === 'review_required') {
		return 'Needs an approving review';
	}
	if (pr.checks.some((check) => check.state === 'failure')) {
		return 'A required check is failing';
	}
	if (pr.checks.some((check) => check.state === 'pending')) {
		return 'Required checks are still running';
	}
	return `The rules of ${pr.baseRef} block the merge`;
}

export function mergeOffer(pr: Subject): MergeOffer {
	if (pr.state === 'draft') {
		return { kind: 'ready' };
	}
	if (pr.state !== 'open') {
		return { kind: 'none' };
	}
	switch (pr.mergeStatus) {
		case 'clean':
			return { kind: 'merge', caution: null };
		case 'unstable':
			return {
				kind: 'merge',
				caution: pr.checks.some((check) => check.state === 'failure')
					? 'Some checks are failing'
					: 'Some checks are still running',
			};
		// GitHub works this out lazily and often hasn't on a first
		// read. It refuses the merge itself if the answer is no.
		case 'unknown':
			return { kind: 'merge', caution: 'GitHub has not said yet whether this can merge' };
		case 'blocked':
			return { kind: 'blocked', reason: blockedReason(pr) };
		case 'behind':
			return { kind: 'blocked', reason: `${pr.headRef} is behind ${pr.baseRef}` };
		case 'conflicting':
			break;
	}
	return { kind: 'blocked', reason: `Conflicts with ${pr.baseRef}` };
}
