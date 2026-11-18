// Which layout a diff surface actually shows, given the user's
// preference and the room it has.

import type { DiffLayout } from '../protocol';

/** Below this width two columns are too cramped to read, so the
 *  diff goes unified whatever the preference says. */
export const NARROW_DIFF_PX = 720;

/** `width` 0 means "not measured yet": the preference stands. */
export function effectiveDiffLayout(preference: DiffLayout, width: number): DiffLayout {
	return preference === 'unified' || (width > 0 && width < NARROW_DIFF_PX) ? 'unified' : 'split';
}

/** Digits the line-number gutters of one diff must fit, so both panes
 *  get the same width. Four covers almost every file and keeps the
 *  code columns of stacked sections on one vertical line. */
export function gutterDigits(...lineCounts: number[]): number {
	return Math.max(4, String(Math.max(1, ...lineCounts)).length);
}
