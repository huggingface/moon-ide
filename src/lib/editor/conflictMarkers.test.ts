import { EditorState } from '@codemirror/state';
import { describe, expect, it } from 'vitest';

import { docContainsConflictMarkers } from './conflictMarkers';

describe('docContainsConflictMarkers', () => {
	it('returns false on a clean buffer', () => {
		const state = EditorState.create({ doc: 'hello\nworld\n' });
		expect(docContainsConflictMarkers(state)).toBe(false);
	});

	it('detects a column-0 `<<<<<<<` marker', () => {
		const state = EditorState.create({ doc: 'pre\n<<<<<<< HEAD\nmine\n=======\ntheirs\n>>>>>>> branch\npost\n' });
		expect(docContainsConflictMarkers(state)).toBe(true);
	});

	it('detects a leftover `=======` even when the other markers were removed', () => {
		// Half-resolved file: user deleted the `<<<<<<<` and
		// `>>>>>>>` lines but missed the separator. We still
		// want the soft-warn to fire — committing this would
		// silently embed the separator in a normal commit.
		const state = EditorState.create({ doc: 'a\n=======\nb\n' });
		expect(docContainsConflictMarkers(state)).toBe(true);
	});

	it('ignores indented or inline marker text', () => {
		// This test file itself contains the marker strings in
		// these very expectations; if the scan didn't require
		// column-0 prefix matching we'd be self-tripping.
		const state = EditorState.create({
			doc: ["// a comment that mentions '<<<<<<<' but is indented", '    >>>>>>> still indented', 'plain text'].join(
				'\n',
			),
		});
		expect(docContainsConflictMarkers(state)).toBe(false);
	});
});
