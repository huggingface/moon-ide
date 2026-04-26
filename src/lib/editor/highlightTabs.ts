import {
	Decoration,
	MatchDecorator,
	ViewPlugin,
	type DecorationSet,
	type ViewUpdate,
	EditorView,
} from '@codemirror/view';

// CM6 ships `highlightWhitespace()` which marks both spaces and tabs. The
// team only wanted tab markers, so we run our own MatchDecorator that
// applies the same `cm-highlightTab` class only to `\t` characters. The
// theme styles that class; the decoration just gets the spans into the DOM.

const tabDeco = Decoration.mark({ class: 'cm-highlightTab' });
const tabMatcher = new MatchDecorator({
	regexp: /\t/g,
	decoration: () => tabDeco,
});

export function highlightTabs() {
	return ViewPlugin.fromClass(
		class {
			decorations: DecorationSet;

			constructor(view: EditorView) {
				this.decorations = tabMatcher.createDeco(view);
			}

			update(update: ViewUpdate) {
				this.decorations = tabMatcher.updateDeco(update, this.decorations);
			}
		},
		{ decorations: (v) => v.decorations },
	);
}
