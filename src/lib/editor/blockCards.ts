// The shell every card dropped between two lines of code shares:
// draft review comments, the composer, and pull-request threads and
// notes. One width rule, one left inset, one surface.
//
// The block widget itself (`.cm-moon-block`) carries the vertical
// spacing as padding, never as margin: CodeMirror measures a block
// widget by its border box, so a margin would be height the editor
// does not know about, and everything laid out from its height map
// (the merge view's alignment above all) would drift by that much
// per card.
//
// The card inside is pinned to the visible pane and never wider than
// it: see `paneMetrics.ts` for where the two lengths come from.

import type { Extension } from '@codemirror/state';
import { EditorView } from '@codemirror/view';
import { paneMetrics } from './paneMetrics';

/** Marks a block widget whose height the other pane of a split diff
 *  has to make room for (`diffBlockAlign.ts`). */
export const BLOCK_CLASS = 'cm-moon-block';

/** Wrap `card` in the block-widget shell. */
export function blockShell(card: HTMLElement): HTMLElement {
	const shell = document.createElement('div');
	shell.className = BLOCK_CLASS;
	card.classList.add('cm-moon-card');
	shell.append(card);
	return shell;
}

const INSET = '28px';
const blockCardsTheme = EditorView.baseTheme({
	'.cm-moon-block': {
		boxSizing: 'border-box',
		padding: '4px 0 6px',
	},
	'.cm-moon-card': {
		position: 'sticky',
		left: `calc(var(--m-gutter-w, 0px) + ${INSET})`,
		boxSizing: 'border-box',
		width: `min(760px, calc(var(--m-pane-w, 100%) - var(--m-gutter-w, 0px) - ${INSET} - 12px))`,
		marginLeft: INSET,
		border: '1px solid var(--m-border)',
		// A card's kind sets `--m-card-accent`; a draft keeps the default.
		borderLeft: '2px solid var(--m-card-accent, var(--m-accent))',
		borderRadius: 'var(--m-radius-md)',
		background: 'var(--m-bg-1)',
		color: 'var(--m-fg)',
		fontFamily: 'var(--m-font-ui)',
		fontSize: 'var(--m-text-sm)',
		whiteSpace: 'normal',
		overflow: 'hidden',
	},
});

export const blockCards: Extension = [paneMetrics, blockCardsTheme];
