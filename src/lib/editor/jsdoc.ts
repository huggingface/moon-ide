import { EditorSelection, Prec, type EditorState } from '@codemirror/state';
import { EditorView, keymap } from '@codemirror/view';

// Small QoL extension for JSDoc-style block comments in JS/TS files.
//
//   1. Autoclose: typing the second `*` so that the line ends with `/**`
//      inserts a matching ` */`, leaving the caret between them. So
//      `/*` + `*` lands you in `/** | */`.
//   2. Continuation: pressing Enter on a line whose trimmed prefix is
//      `/**` or `*` (and we're inside an open `/** ... */` block) starts
//      the new line with ` * ` at the same indent, mirroring how IDEs
//      like VS Code continue JSDoc. If the closer `*/` sits right after
//      the caret, we expand to a three-line block and park the caret on
//      the middle line.
//
// Both behaviours are scoped to the JS/TS branch of `language.ts`, so
// they only run inside files that actually have JSDoc.

// Match the leading whitespace of a line.
const LEADING_WS_RE = /^[ \t]*/;

// True when the rest of the current line after `pos` already contains a
// `*/` closer. Used to skip autoclose if the user is editing inside an
// existing block comment that already has its terminator.
function lineAlreadyClosed(state: EditorState, pos: number): boolean {
	const line = state.doc.lineAt(pos);
	const after = state.doc.sliceString(pos, line.to);
	return after.includes('*/');
}

// Autoclose `/**` → `/** | */` on the second `*`.
//
// CM dispatches the keystroke through `inputHandler` *before* the
// character is inserted; we own the dispatch when we return `true`,
// and produce a single transaction that inserts `*` + ` */` and parks
// the caret right after the `**`.
const autocloseOpener = EditorView.inputHandler.of((view, from, to, text) => {
	if (text !== '*' || from !== to) {
		return false;
	}
	const { state } = view;
	// The doc still has the single `*` at this point; check that what
	// sits before the caret is `/*`, so after our `*` it becomes `/**`.
	const line = state.doc.lineAt(from);
	const before = state.doc.sliceString(line.from, from);
	if (!/^[ \t]*\/\*$/.test(before)) {
		return false;
	}
	if (lineAlreadyClosed(state, from)) {
		return false;
	}
	view.dispatch({
		changes: { from, to, insert: '* */' },
		selection: EditorSelection.cursor(from + 1),
		userEvent: 'input.type',
		scrollIntoView: true,
	});
	return true;
});

// Enter inside a JSDoc block continues the comment with ` * ` at the
// matching indent.
function continueDocCommentOnEnter(view: EditorView): boolean {
	const { state } = view;
	const { main } = state.selection;
	if (!main.empty) {
		return false;
	}
	const pos = main.from;
	const line = state.doc.lineAt(pos);
	const before = state.doc.sliceString(line.from, pos);
	const after = state.doc.sliceString(pos, line.to);

	const indent = LEADING_WS_RE.exec(line.text)?.[0] ?? '';
	const trimmedBefore = before.slice(indent.length);

	// Opener line: `/**` (optionally followed by content) — continue
	// only if the block is still open. We treat "still open" as: the
	// text from the opener onward, plus the rest of the document up
	// to the next `*/`, contains no nested `/*` reopener. A cheap
	// heuristic that's correct for normal JSDoc and gives up
	// gracefully when it isn't.
	const openerMatch = /^\/\*\*(?!\/)/.test(trimmedBefore);
	const continuationMatch = /^\*(?!\/)/.test(trimmedBefore);
	if (!openerMatch && !continuationMatch) {
		return false;
	}
	if (!insideOpenDocBlock(state, pos)) {
		return false;
	}

	// If the closer sits right after the caret on the same line
	// (typical for the autoclosed `/** | */`), expand to a 3-line
	// block and park the caret in the middle.
	const closerImmediate = /^\s*\*\//.test(after);
	const newlinePrefix = `\n${indent} * `;
	if (closerImmediate && openerMatch) {
		const insert = `${newlinePrefix}\n${indent} `;
		view.dispatch({
			changes: { from: pos, to: pos, insert },
			selection: EditorSelection.cursor(pos + newlinePrefix.length),
			userEvent: 'input',
			scrollIntoView: true,
		});
		return true;
	}

	view.dispatch({
		changes: { from: pos, to: pos, insert: newlinePrefix },
		selection: EditorSelection.cursor(pos + newlinePrefix.length),
		userEvent: 'input',
		scrollIntoView: true,
	});
	return true;
}

// Scan backwards from `pos` for `/**` (with no intervening `*/`), and
// forwards for `*/` (with no intervening `/*` reopener). When we find
// an opener and the closer is either absent or sits after the
// continuation we're about to insert, we're inside an open block.
function insideOpenDocBlock(state: EditorState, pos: number): boolean {
	const text = state.doc.toString();
	// Walk backwards to find the nearest `/**` that isn't preceded by
	// a closing `*/`.
	let openerAt = -1;
	let i = pos - 1;
	while (i >= 1) {
		const two = text.charCodeAt(i - 1) === 0x2f && text.charCodeAt(i) === 0x2a; // `/*`
		const close = text.charCodeAt(i - 1) === 0x2a && text.charCodeAt(i) === 0x2f; // `*/`
		if (close) {
			return false;
		}
		if (two) {
			// `/*` found; require a third `*` to qualify as JSDoc.
			if (text.charCodeAt(i + 1) === 0x2a) {
				openerAt = i - 1;
			}
			break;
		}
		i--;
	}
	if (openerAt < 0) {
		return false;
	}
	return true;
}

// `Prec.high` so we beat `defaultKeymap`'s `insertNewlineAndIndent`.
// The autocomplete keymap also runs at `Prec.high` and is registered
// earlier in the extension array, so it still gets first crack at
// Enter when the completion popup is open (and falls through here when
// it isn't).
const continueKeymap = Prec.high(keymap.of([{ key: 'Enter', run: continueDocCommentOnEnter }]));

export function jsdocExtension() {
	return [autocloseOpener, continueKeymap];
}

// Exposed for unit tests so we don't have to spin up an EditorView.
export const __test = { insideOpenDocBlock };
