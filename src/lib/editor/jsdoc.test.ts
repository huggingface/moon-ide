import { describe, expect, it } from 'vitest';

import { EditorState } from '@codemirror/state';

import { __test } from './jsdoc';

const { insideOpenDocBlock } = __test;

function stateOf(text: string): EditorState {
	return EditorState.create({ doc: text });
}

describe('insideOpenDocBlock', () => {
	it('returns true when caret sits right after `/**`', () => {
		const text = '/**';
		expect(insideOpenDocBlock(stateOf(text), text.length)).toBe(true);
	});

	it('returns true for caret inside an open block', () => {
		const text = '/**\n * hello';
		expect(insideOpenDocBlock(stateOf(text), text.length)).toBe(true);
	});

	it('returns false when the block is already closed before the caret', () => {
		const text = '/** done */\nconst x = 1;';
		expect(insideOpenDocBlock(stateOf(text), text.length)).toBe(false);
	});

	it('returns false for a plain `/*` (non-JSDoc) block', () => {
		const text = '/* plain';
		expect(insideOpenDocBlock(stateOf(text), text.length)).toBe(false);
	});

	it('returns false when there is no opener at all', () => {
		const text = 'const x = 1;';
		expect(insideOpenDocBlock(stateOf(text), text.length)).toBe(false);
	});
});
