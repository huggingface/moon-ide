import { describe, expect, it } from 'vitest';

import { EditorState } from '@codemirror/state';

import { reanchorComments, reviewLineFingerprint } from './reviewComments';
import type { ReviewComment } from '../protocol';

function stateOf(src: string): EditorState {
	return EditorState.create({ doc: src });
}

function comment(startLine: number, endLine: number, lineText: string): ReviewComment {
	return {
		id: `c-${startLine}-${endLine}`,
		anchor: {
			path: 'src/lib.rs',
			side: 'working',
			startLine,
			endLine,
			fingerprint: reviewLineFingerprint(lineText),
			baselineRev: 'HEAD',
		},
		body: 'looks off',
		createdAt: new Date().toISOString(),
	};
}

describe('reviewLineFingerprint', () => {
	it('ignores leading/trailing indentation so a reformat keeps the anchor', () => {
		expect(reviewLineFingerprint('\tlet x = 1;')).toBe(reviewLineFingerprint('    let x = 1;  '));
	});

	it('differs when the meaningful content differs', () => {
		expect(reviewLineFingerprint('let x = 1;')).not.toBe(reviewLineFingerprint('let x = 2;'));
	});

	it('is stable for multi-line ranges joined by newline', () => {
		const a = reviewLineFingerprint('foo\nbar');
		const b = reviewLineFingerprint('  foo  \n\tbar');
		expect(a).toBe(b);
	});
});

describe('reanchorComments', () => {
	it('reports no move when the hint line still matches', () => {
		const src = ['fn a() {}', 'fn b() {}', 'fn c() {}'].join('\n');
		const c = comment(2, 2, 'fn b() {}');
		expect(reanchorComments(stateOf(src), [c])).toEqual([]);
	});

	it('re-pins to the new line when content shifted down (lines inserted above)', () => {
		// `fn b()` was on line 2; two lines got prepended, so it's now
		// on line 4. The hint still says 2.
		const src = ['// new', '// new', 'fn a() {}', 'fn b() {}', 'fn c() {}'].join('\n');
		const c = comment(2, 2, 'fn b() {}');
		expect(reanchorComments(stateOf(src), [c])).toEqual([{ id: 'c-2-2', startLine: 4, endLine: 4 }]);
	});

	it('re-pins when content shifted up (lines removed above)', () => {
		// `fn c()` was on line 5 originally; two lines removed above
		// put it on line 3. Hint still says 5.
		const src = ['fn a() {}', 'fn b() {}', 'fn c() {}'].join('\n');
		const c = comment(5, 5, 'fn c() {}');
		expect(reanchorComments(stateOf(src), [c])).toEqual([{ id: 'c-5-5', startLine: 3, endLine: 3 }]);
	});

	it('leaves a stale comment untouched (fingerprint gone)', () => {
		// The anchored line text no longer exists anywhere — the line
		// was edited out from under the comment. No re-pin emitted;
		// the renderer shows it as stale.
		const src = ['fn a() {}', 'fn totally_different() {}', 'fn c() {}'].join('\n');
		const c = comment(2, 2, 'fn b() {}');
		expect(reanchorComments(stateOf(src), [c])).toEqual([]);
	});

	it('handles a multi-line anchor that drifted', () => {
		const src = ['head', 'head', 'first', 'second', 'tail'].join('\n');
		// `first` + `second` were originally on lines 1-2; now 3-4.
		const c = comment(1, 2, 'first\nsecond');
		expect(reanchorComments(stateOf(src), [c])).toEqual([{ id: 'c-1-2', startLine: 3, endLine: 4 }]);
	});
});
