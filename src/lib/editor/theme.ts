import { EditorView } from '@codemirror/view';

// Minimal CM6 theme that reads the same CSS variables as the rest of the app
// so toggling light/dark on :root just works.
export const moonTheme = EditorView.theme(
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
			color: '#0d1017',
		},
		'.cm-scroller': {
			fontFamily: 'var(--m-font-mono)',
		},
		// Easter-egg moon icon in the scrollbar corner (the otherwise-
		// white square where the vertical and horizontal scrollbars
		// meet). Known issue: WebKitGTK rasterizes scrollbar pseudo-
		// elements once and does not repaint them on subsequent style
		// changes — not via CSS variable updates, ancestor class flips,
		// stylesheet swaps, forced reflow, or `display: none` cycles
		// (we tried all of them in commit history; nothing took). So
		// after a theme toggle the corner stays at its first-paint
		// colour. We accept this until we move off WebKitGTK or find a
		// real invalidation path; if we ever do, the matching theme-
		// flip wiring used to live in `applyScrollbarTheme()` in
		// `lib/state.svelte.ts` (commit history). The fill is hardcoded
		// because data: URLs can't read CSS custom properties.
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
		// Tab markers (always on; see `lib/editor/highlightTabs.ts`).
		// Small left-anchored `→`. Color is hardcoded in the SVG (encoded
		// `#5a6480` = `--m-fg-subtle` in dark) because data: URLs cannot
		// read CSS variables. Light theme will reuse the same color until
		// we ship proper editor-chrome theme switching. We previously
		// also marked spaces, but the dots were too noisy on this theme —
		// see ADR-style note in `highlightTabs.ts`.
		'.cm-highlightTab': {
			backgroundImage:
				"url(\"data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 16 16'%3E%3Cpath d='M2 8h10M9 5l3 3-3 3' stroke='%235a6480' stroke-width='1.5' fill='none' stroke-linecap='round' stroke-linejoin='round'/%3E%3C/svg%3E\")",
			backgroundRepeat: 'no-repeat',
			backgroundPosition: 'left center',
			backgroundSize: '1ch auto',
		},
	},
	{ dark: true },
);
