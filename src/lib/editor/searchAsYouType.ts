// Search-as-you-type panel for CodeMirror.
//
// The stock `@codemirror/search` panel updates the live match
// highlights as you type, but only *moves the selection* to a match
// when you press Enter (or click "next"). The team wants the editor to
// jump to the first match the moment a query is typed, the way the
// browser find bar and VS Code's find widget behave.
//
// We keep the default panel's DOM and feature set (case / regexp /
// whole-word toggles, replace fields, next/prev/all buttons) but
// override the panel implementation via `search({ createPanel })`. The
// only behavioural change is in `commit()`: after pushing the new
// query we move the selection to the first match at or after a stable
// anchor. The anchor is captured when the panel opens, so typing more
// characters refines the same match rather than walking the caret
// forward through the document on every keystroke.

import { EditorSelection, type Extension } from '@codemirror/state';
import { EditorView, type Panel, runScopeHandlers } from '@codemirror/view';
import {
	closeSearchPanel,
	findNext,
	findPrevious,
	getSearchQuery,
	replaceAll,
	replaceNext,
	search,
	SearchQuery,
	selectMatches,
	setSearchQuery,
} from '@codemirror/search';

function phrase(view: EditorView, text: string): string {
	return view.state.phrase(text);
}

function button(name: string, onclick: () => void, label: string): HTMLButtonElement {
	const node = document.createElement('button');
	node.className = 'cm-button';
	node.name = name;
	node.type = 'button';
	node.textContent = label;
	node.addEventListener('click', onclick);
	return node;
}

function textfield(name: string, value: string, label: string, main: boolean): HTMLInputElement {
	const node = document.createElement('input');
	node.className = 'cm-textfield';
	node.name = name;
	node.value = value;
	node.placeholder = label;
	node.setAttribute('aria-label', label);
	if (main) {
		node.setAttribute('main-field', 'true');
	}
	return node;
}

function checkbox(name: string, checked: boolean): HTMLInputElement {
	const node = document.createElement('input');
	node.type = 'checkbox';
	node.name = name;
	node.checked = checked;
	return node;
}

function labelled(field: HTMLElement, text: string): HTMLLabelElement {
	const node = document.createElement('label');
	node.append(field, text);
	return node;
}

class SearchAsYouTypePanel implements Panel {
	readonly dom: HTMLElement;
	private query: SearchQuery;
	// Position the live "first match" search anchors to. Captured when
	// the panel mounts so refining the query doesn't march the caret
	// forward through the doc on every keystroke.
	private anchor: number;
	private readonly searchField: HTMLInputElement;
	private readonly replaceField: HTMLInputElement;
	private readonly caseField: HTMLInputElement;
	private readonly reField: HTMLInputElement;
	private readonly wordField: HTMLInputElement;

	constructor(private readonly view: EditorView) {
		this.query = getSearchQuery(view.state);
		this.anchor = view.state.selection.main.from;

		const commit = (): void => {
			this.commit();
		};

		this.searchField = textfield('search', this.query.search, phrase(view, 'Find'), true);
		this.searchField.addEventListener('input', commit);
		this.replaceField = textfield('replace', this.query.replace, phrase(view, 'Replace'), false);
		this.caseField = checkbox('case', this.query.caseSensitive);
		this.caseField.addEventListener('change', commit);
		this.reField = checkbox('re', this.query.regexp);
		this.reField.addEventListener('change', commit);
		this.wordField = checkbox('word', this.query.wholeWord);
		this.wordField.addEventListener('change', commit);

		const close = document.createElement('button');
		close.name = 'close';
		close.type = 'button';
		close.textContent = '×';
		close.setAttribute('aria-label', phrase(view, 'close'));
		close.addEventListener('click', () => closeSearchPanel(view));

		this.dom = document.createElement('div');
		this.dom.className = 'cm-search';
		this.dom.addEventListener('keydown', (e) => {
			this.keydown(e);
		});
		this.dom.append(
			this.searchField,
			button('next', () => findNext(view), phrase(view, 'next')),
			button('prev', () => findPrevious(view), phrase(view, 'previous')),
			button('select', () => selectMatches(view), phrase(view, 'all')),
			labelled(this.caseField, phrase(view, 'match case')),
			labelled(this.reField, phrase(view, 'regexp')),
			labelled(this.wordField, phrase(view, 'by word')),
		);
		if (!view.state.readOnly) {
			this.dom.append(
				document.createElement('br'),
				this.replaceField,
				button('replace', () => replaceNext(view), phrase(view, 'replace')),
				button('replaceAll', () => replaceAll(view), phrase(view, 'replace all')),
			);
		}
		this.dom.append(close);
	}

	private commit(): void {
		const query = new SearchQuery({
			search: this.searchField.value,
			caseSensitive: this.caseField.checked,
			regexp: this.reField.checked,
			wholeWord: this.wordField.checked,
			replace: this.replaceField.value,
		});
		if (query.eq(this.query)) {
			return;
		}
		this.query = query;
		this.view.dispatch({ effects: setSearchQuery.of(query) });
		this.jumpToFirstMatch(query);
	}

	// Move the selection to the first match at or after the anchor,
	// wrapping to the document start if nothing follows. Leaves the
	// selection untouched (no jarring jump) when the query is empty or
	// invalid — e.g. a half-typed regexp.
	private jumpToFirstMatch(query: SearchQuery): void {
		if (!query.valid) {
			return;
		}
		const { state } = this.view;
		let next = query.getCursor(state, this.anchor).next();
		if (next.done) {
			next = query.getCursor(state, 0, this.anchor).next();
			if (next.done) {
				return;
			}
		}
		const { from, to } = next.value;
		this.view.dispatch({
			selection: EditorSelection.single(from, to),
			effects: EditorView.scrollIntoView(from, { y: 'center' }),
			userEvent: 'select.search',
		});
	}

	private keydown(e: KeyboardEvent): void {
		if (runScopeHandlers(this.view, e, 'search-panel')) {
			e.preventDefault();
			return;
		}
		if (e.keyCode === 13 && e.target === this.searchField) {
			// Enter / Shift+Enter step through matches from the
			// *current* selection, same as the stock panel — this is
			// how the user advances past the live first-match jump.
			e.preventDefault();
			(e.shiftKey ? findPrevious : findNext)(this.view);
			return;
		}
		if (e.keyCode === 13 && e.target === this.replaceField) {
			e.preventDefault();
			replaceNext(this.view);
		}
	}

	update(): void {
		// The query is only ever mutated from this panel's own fields,
		// so there's nothing external to react to. (The stock panel
		// syncs from `setSearchQuery` effects fired elsewhere; we don't
		// fire any.)
	}

	mount(): void {
		// Re-anchor each time the panel opens so a fresh Ctrl+F starts
		// searching from where the caret currently sits.
		this.anchor = this.view.state.selection.main.from;
		this.searchField.select();
	}

	get pos(): number {
		return 80;
	}

	get top(): boolean {
		return false;
	}
}

export function searchAsYouType(): Extension {
	return search({ createPanel: (view) => new SearchAsYouTypePanel(view) });
}
