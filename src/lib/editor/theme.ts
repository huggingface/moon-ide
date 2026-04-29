import { EditorView } from '@codemirror/view';
import { HighlightStyle, syntaxHighlighting } from '@codemirror/language';
import type { Extension } from '@codemirror/state';
import { tags as t } from '@lezer/highlight';

// Editor chrome (background, gutter, selection, panels, tooltips).
//
// Most colors come from CSS variables defined in `styles.css`, so toggling
// `.light` on `:root` re-skins everything for free. The one thing we
// *can't* do via CSS is the `dark: boolean` flag CodeMirror itself reads
// — it picks different built-in defaults for things like the autocomplete
// hover and drop cursor based on it. So `moonTheme(dark)` rebuilds the
// extension when the user flips theme; the Editor wraps the result in a
// `Compartment` and reconfigures on theme change.
function moonTheme(dark: boolean): Extension {
	return EditorView.theme(
		{
			'&': {
				color: 'var(--m-fg)',
				backgroundColor: 'var(--m-editor-bg)',
				height: '100%',
				fontSize: '13px',
				fontFamily: 'var(--m-font-mono)',
			},
			'.cm-content': {
				caretColor: 'var(--m-accent)',
				padding: '8px 0',
			},
			'.cm-gutters': {
				backgroundColor: 'var(--m-editor-bg)',
				color: 'var(--m-fg-subtle)',
				border: 'none',
				borderRight: '1px solid var(--m-border)',
			},
			'.cm-activeLineGutter': {
				backgroundColor: 'transparent',
				color: 'var(--m-fg)',
			},
			'.cm-activeLine': {
				backgroundColor: 'var(--m-editor-line-active)',
			},
			'&.cm-focused .cm-cursor': {
				borderLeftColor: 'var(--m-accent)',
			},
			'&.cm-focused .cm-selectionBackground, ::selection': {
				backgroundColor: 'var(--m-editor-selection)',
			},
			'.cm-selectionBackground': {
				backgroundColor: 'var(--m-editor-selection)',
			},
			'.cm-tooltip': {
				backgroundColor: 'var(--m-bg-2)',
				borderColor: 'var(--m-border-strong)',
				color: 'var(--m-fg)',
			},
			'.cm-tooltip.cm-tooltip-autocomplete > ul > li[aria-selected]': {
				backgroundColor: 'var(--m-accent)',
				color: 'var(--m-bg)',
			},
			// Search panel (Ctrl+F) and goto-line panel. CM6 ships its
			// own light-grey defaults that look out of place in either
			// theme; we override them with our palette tokens. Inputs
			// and buttons inside the panel inherit `color` from the
			// panel itself, so they pick up `--m-fg` automatically.
			'.cm-panels': {
				backgroundColor: 'var(--m-bg-1)',
				color: 'var(--m-fg)',
			},
			'.cm-panels.cm-panels-top': {
				borderBottom: '1px solid var(--m-border)',
			},
			'.cm-panels.cm-panels-bottom': {
				borderTop: '1px solid var(--m-border)',
			},
			'.cm-panel.cm-search': {
				padding: '4px 6px',
			},
			'.cm-panel.cm-search input, .cm-panel.cm-search [name=search], .cm-textfield': {
				backgroundColor: 'var(--m-bg-2)',
				color: 'var(--m-fg)',
				border: '1px solid var(--m-border)',
				borderRadius: '3px',
				padding: '2px 6px',
			},
			'.cm-panel.cm-search input:focus, .cm-textfield:focus': {
				outline: '1px solid var(--m-accent)',
				outlineOffset: '0',
				borderColor: 'var(--m-accent)',
			},
			'.cm-panel.cm-search button, .cm-button': {
				backgroundColor: 'transparent',
				backgroundImage: 'none',
				color: 'var(--m-fg)',
				border: '1px solid var(--m-border)',
				borderRadius: '3px',
				padding: '2px 8px',
				margin: '0 2px',
				cursor: 'pointer',
			},
			'.cm-panel.cm-search button:hover, .cm-button:hover': {
				backgroundColor: 'var(--m-bg-overlay)',
				borderColor: 'var(--m-border-strong)',
			},
			'.cm-panel.cm-search label': {
				color: 'var(--m-fg-muted)',
				fontSize: '12px',
			},
			'.cm-panel.cm-search [name=close]': {
				color: 'var(--m-fg-muted)',
			},
			'.cm-scroller': {
				fontFamily: 'var(--m-font-mono)',
			},
			// Easter-egg moon icon in the scrollbar corner — see the
			// big comment by the rule itself for the WebKitGTK caveat.
			'.cm-scroller::-webkit-scrollbar-corner': {
				backgroundColor: 'var(--m-editor-bg)',
				backgroundImage:
					"url(\"data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 16 16'%3E%3Cpath fill-rule='evenodd' fill='%235a6480' d='M8 2a6 6 0 1 0 0 12A6 6 0 1 0 8 2zM10 4a4 4 0 1 0 0 8A4 4 0 1 0 10 4z'/%3E%3C/svg%3E\")",
				backgroundRepeat: 'no-repeat',
				backgroundPosition: 'center',
				backgroundSize: '10px 10px',
			},
			'.cm-searchMatch': {
				backgroundColor: 'rgba(240, 184, 110, 0.25)',
				outline: '1px solid rgba(240, 184, 110, 0.6)',
			},
			'.cm-searchMatch.cm-searchMatch-selected': {
				backgroundColor: 'rgba(240, 184, 110, 0.45)',
			},
			// Tab markers (always on; see `lib/editor/highlightTabs.ts`).
			// Color is hardcoded in the SVG (encoded `#5a6480`) because
			// data: URLs cannot read CSS variables — light theme reuses
			// the same color until we ship proper editor-chrome theme
			// switching. We previously also marked spaces, but the dots
			// were too noisy on this theme.
			'.cm-highlightTab': {
				backgroundImage:
					"url(\"data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 16 16'%3E%3Cpath d='M2 8h10M9 5l3 3-3 3' stroke='%235a6480' stroke-width='1.5' fill='none' stroke-linecap='round' stroke-linejoin='round'/%3E%3C/svg%3E\")",
				backgroundRepeat: 'no-repeat',
				backgroundPosition: 'left center',
				backgroundSize: '1ch auto',
			},
		},
		{ dark },
	);
}

// Syntax palette. Defined once; colors come from CSS variables so a
// theme flip on `:root` is enough to repaint without rebuilding the
// HighlightStyle. We cover the full set of common Lezer tags rather
// than rely on inheritance fallbacks — anything we miss falls back to
// `--m-fg`, which is fine but visually flat.
const moonHighlight = HighlightStyle.define([
	{
		tag: [t.comment, t.lineComment, t.blockComment, t.docComment, t.docString],
		color: 'var(--m-syntax-comment)',
		fontStyle: 'italic',
	},
	{
		tag: [
			t.keyword,
			t.controlKeyword,
			t.operatorKeyword,
			t.modifier,
			t.definitionKeyword,
			t.moduleKeyword,
			t.self,
			t.null,
		],
		color: 'var(--m-syntax-keyword)',
	},
	{ tag: [t.string, t.special(t.string), t.character], color: 'var(--m-syntax-string)' },
	{ tag: [t.escape, t.regexp], color: 'var(--m-syntax-regexp)' },
	{
		tag: [t.number, t.integer, t.float, t.bool, t.atom, t.literal, t.unit],
		color: 'var(--m-syntax-number)',
	},
	{
		tag: [t.function(t.variableName), t.function(t.propertyName), t.function(t.definition(t.variableName))],
		color: 'var(--m-syntax-function)',
	},
	{
		tag: [t.typeName, t.className, t.namespace, t.macroName],
		color: 'var(--m-syntax-type)',
	},
	{ tag: [t.propertyName, t.attributeName], color: 'var(--m-syntax-property)' },
	{ tag: t.attributeValue, color: 'var(--m-syntax-attribute-value)' },
	{ tag: [t.tagName, t.angleBracket], color: 'var(--m-syntax-tag)' },
	{
		tag: [t.constant(t.variableName), t.standard(t.variableName), t.labelName],
		color: 'var(--m-syntax-constant)',
	},
	{
		tag: [
			t.operator,
			t.arithmeticOperator,
			t.bitwiseOperator,
			t.compareOperator,
			t.controlOperator,
			t.definitionOperator,
			t.derefOperator,
			t.logicOperator,
			t.typeOperator,
			t.updateOperator,
			t.punctuation,
			t.bracket,
			t.brace,
			t.paren,
			t.squareBracket,
			t.separator,
		],
		color: 'var(--m-syntax-operator)',
	},
	{
		tag: [t.meta, t.documentMeta, t.processingInstruction, t.annotation],
		color: 'var(--m-syntax-meta)',
		fontStyle: 'italic',
	},
	// Markdown
	{
		tag: [t.heading, t.heading1, t.heading2, t.heading3, t.heading4, t.heading5, t.heading6],
		color: 'var(--m-syntax-heading)',
		fontWeight: 'bold',
	},
	{ tag: t.emphasis, fontStyle: 'italic' },
	{ tag: t.strong, fontWeight: 'bold' },
	{ tag: t.strikethrough, textDecoration: 'line-through' },
	{ tag: [t.link, t.url], color: 'var(--m-syntax-link)', textDecoration: 'underline' },
	{ tag: t.quote, color: 'var(--m-syntax-comment)', fontStyle: 'italic' },
	{ tag: t.list, color: 'var(--m-syntax-keyword)' },
	{ tag: t.contentSeparator, color: 'var(--m-syntax-operator)' },
	{ tag: t.monospace, color: 'var(--m-syntax-string)' },
	// Diff-flavored buffers (e.g. patch files later).
	{ tag: t.inserted, color: 'var(--m-syntax-inserted)' },
	{ tag: t.deleted, color: 'var(--m-syntax-deleted)' },
	{ tag: t.changed, color: 'var(--m-syntax-changed)' },
	{
		tag: t.invalid,
		color: 'var(--m-syntax-invalid)',
		textDecoration: 'underline wavy',
	},
]);

// Bundled extension the Editor reconfigures whenever `workspace.theme`
// flips. Includes both the chrome theme and the syntax highlighter so
// they stay in lockstep.
export function moonEditorTheme(mode: 'dark' | 'light'): Extension[] {
	return [moonTheme(mode === 'dark'), syntaxHighlighting(moonHighlight)];
}
