// Pure text-edit helpers extracted from `lspRename.ts`.
//
// `lspRename.ts` itself transitively imports `state.svelte` (it
// dispatches workspace edits through the active folder host), and
// pulling the Svelte runtime into a `bun test` execution context
// fails at module load with `$state is not defined`. Keeping the
// pure offset / sort / splice logic here lets the unit tests
// exercise it without bootstrapping the entire app.

import type { LspPosition, LspTextEdit } from '../protocol';

/**
 * Apply a batch of LSP text edits to a string. Edits are sorted
 * by descending start position before splicing so earlier edits'
 * offsets aren't affected by later replacements.
 */
export function applyEditsToText(original: string, edits: readonly LspTextEdit[]): string {
	const lineStarts = lineStartsOf(original);
	const sorted = edits.toSorted((a, b) => {
		if (a.range.start.line !== b.range.start.line) {
			return b.range.start.line - a.range.start.line;
		}
		return b.range.start.character - a.range.start.character;
	});
	let text = original;
	for (const edit of sorted) {
		const from = offsetForPosition(original.length, lineStarts, edit.range.start);
		const to = offsetForPosition(original.length, lineStarts, edit.range.end);
		text = text.slice(0, from) + edit.newText + text.slice(to);
	}
	return text;
}

export function lineStartsOf(text: string): number[] {
	const starts: number[] = [0];
	for (let i = 0; i < text.length; i++) {
		// Codepoint 10 (`\n`) is the only line terminator LSP
		// positions are spec'd against. Servers normalise
		// `\r\n` to `\n` when computing positions; we mirror
		// that by ignoring `\r`.
		if (text.charCodeAt(i) === 10) {
			starts.push(i + 1);
		}
	}
	return starts;
}

export function offsetForPosition(textLength: number, lineStarts: readonly number[], pos: LspPosition): number {
	if (pos.line < 0) {
		return 0;
	}
	const start = lineStarts[pos.line];
	if (start === undefined) {
		return textLength;
	}
	return start + pos.character;
}
