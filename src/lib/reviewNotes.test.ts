import { describe, expect, it } from 'vitest';
import { addNote, askAt, dismiss, explainPrompt, replyPrompt, settle, type NoteThread } from './reviewNotes';

const A = { path: 'src/a.ts', side: 'working', line: 10 } as const;
const B = { path: 'src/b.ts', side: 'working', line: 3 } as const;

describe('review notes', () => {
	it('answers an "explain" on the line that was asked about', () => {
		const asked = askAt([], A, null);
		expect(asked).toEqual([{ ...A, entries: [], pending: true }]);
		const answered = addNote(asked, A, 'Takes the lock first.');
		expect(answered).toEqual([{ ...A, entries: [{ from: 'agent', text: 'Takes the lock first.' }], pending: false }]);
	});

	it('keeps a thread going: reply, then the next note', () => {
		let threads = addNote([], A, 'First.');
		threads = askAt(threads, A, 'Why not skip it?');
		expect(threads[0]?.pending).toBe(true);
		threads = addNote(threads, A, 'Because of the reuse check.');
		expect(threads[0]?.entries.map((entry) => entry.from)).toEqual(['agent', 'you', 'agent']);
	});

	it('leaves other threads untouched, by identity', () => {
		const before = addNote(addNote([], A, 'a'), B, 'b');
		const after = addNote(before, B, 'more');
		expect(after[0]).toBe(before[0]);
		expect(after[1]).not.toBe(before[1]);
	});

	it('puts a note on another line in its own thread, same line other side too', () => {
		const threads = addNote(addNote([], A, 'new side'), { ...A, side: 'base' }, 'old side');
		expect(threads).toHaveLength(2);
	});

	it('settles what the agent left unanswered', () => {
		const answered = addNote([], A, 'a');
		const threads: readonly NoteThread[] = askAt(askAt(answered, A, 'and?'), B, null);
		const settled = settle(threads);
		expect(settled).toEqual([{ ...A, entries: threads[0]?.entries, pending: false }]);
		expect(settle(answered)).toBe(answered);
	});

	it('dismisses one thread', () => {
		expect(dismiss(addNote(addNote([], A, 'a'), B, 'b'), A)).toEqual([expect.objectContaining(B)]);
	});

	it('words the requests to the agent', () => {
		const pr = { number: 7, title: 'Lock cards' };
		expect(explainPrompt(pr, 'src/a.ts', { from: 10, to: 24 })).toContain('`src/a.ts` lines 10-24');
		expect(explainPrompt(pr, 'src/a.ts', { from: 10, to: 24 })).toContain('`review_note` (path `src/a.ts`, line 10)');
		expect(explainPrompt(pr, 'src/a.ts', null)).toContain('the first changed line');
		const thread = addNote([], A, 'a')[0];
		expect(thread && replyPrompt(thread, 'Why?')).toContain('`src/a.ts` line 10: Why?');
	});
});
