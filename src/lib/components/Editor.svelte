<script lang="ts">
	import { onMount } from 'svelte';
	import { EditorState, Compartment } from '@codemirror/state';
	import { EditorView, highlightActiveLine, highlightActiveLineGutter, keymap, lineNumbers } from '@codemirror/view';
	import { highlightTabs } from '../editor/highlightTabs';
	import { defaultKeymap, history, historyKeymap, indentWithTab } from '@codemirror/commands';
	import { searchKeymap, highlightSelectionMatches } from '@codemirror/search';
	import { bracketMatching, indentOnInput, indentUnit } from '@codemirror/language';
	import { autocompletion, closeBrackets, closeBracketsKeymap, completionKeymap } from '@codemirror/autocomplete';
	import {
		applyDiagnostics,
		filePathFacet,
		lspCompletionSource,
		lspDiagnosticsExtension,
		lspHoverExtension,
	} from '../editor/lsp';
	import { workspace, type OpenFile, type SplitSide } from '../state.svelte';
	import { languageFor } from '../editor/language';
	import { moonEditorTheme } from '../editor/theme';
	import { type EditorConfig } from '../protocol';

	type Props = { file: OpenFile; side: SplitSide };
	let { file, side }: Props = $props();

	let host: HTMLDivElement;
	let view: EditorView | undefined;
	const languageCompartment = new Compartment();
	// Editorconfig settings live in their own compartment because they
	// can change without the file changing — saving a `.editorconfig`
	// invalidates the resolved settings for every open buffer.
	const editorConfigCompartment = new Compartment();
	// Theme + syntax highlight live together: CodeMirror reads a
	// `dark: boolean` flag on the chrome theme that we can't fake from
	// CSS, so flipping `workspace.theme` rebuilds both.
	const themeCompartment = new Compartment();
	// Current buffer path, exposed to LSP adapters via `filePathFacet`.
	// Reconfigured whenever the active tab swaps so the hover and
	// completion sources always talk about the file the user is
	// looking at.
	const lspPathCompartment = new Compartment();

	// Each Editor instance owns one CM view that we re-target as the active file changes.
	// We track the path the view currently holds so we know when to swap state.
	let currentPath: string | null = null;

	onMount(() => {
		void workspace.ensureEditorConfig(file.path);
		const state = EditorState.create({
			doc: file.text,
			extensions: baseExtensions(),
		});
		view = new EditorView({ state, parent: host });
		currentPath = file.path;
		void applyLanguage(file.path, file.text);
		return () => {
			view?.destroy();
			view = undefined;
		};
	});

	$effect(() => {
		// Reactive dependency: any save-as bumps `renameTick` so this
		// effect re-runs even if `file` is the same object reference
		// (Svelte 5 wouldn't otherwise notice the path field changing
		// on an existing buffer because we replace the object via map).
		workspace.renameTick;
		const v = view;
		if (!v) {
			return;
		}
		if (file.path !== currentPath) {
			// "Save As" / first save of an untitled buffer rebinds the
			// path on the same `OpenFile`; preserve selection, scroll,
			// and undo history. We trust an explicit rename signal
			// rather than content equality because the pre-save
			// pipeline (final newline, trailing-whitespace trim, line
			// endings) can leave the freshly-read text differing from
			// the live view doc — content equality would silently
			// mis-classify those saves as tab switches and reset state.
			const renamed = workspace.isRename(currentPath, file.path);
			currentPath = file.path;
			void workspace.ensureEditorConfig(file.path);
			void applyLanguage(file.path, file.text);
			if (renamed) {
				// Pipeline may have rewritten the bytes; sync the doc
				// without rebuilding state.
				if (file.text !== v.state.doc.toString()) {
					v.dispatch({
						changes: { from: 0, to: v.state.doc.length, insert: file.text },
					});
				}
				return;
			}
			const next = EditorState.create({
				doc: file.text,
				extensions: baseExtensions(),
			});
			v.setState(next);
			return;
		}
		// Same path, but the in-memory text may differ if state was mutated externally.
		if (file.text !== v.state.doc.toString()) {
			v.dispatch({
				changes: { from: 0, to: v.state.doc.length, insert: file.text },
			});
		}
	});

	// Reactive: when the resolved editorconfig for the active file
	// changes (first fetch, or refresh after the user saved a
	// `.editorconfig`), reconfigure the compartment in place. We don't
	// rebuild the editor state — only tabSize / indentUnit need to flip.
	$effect(() => {
		const ec = workspace.editorConfigFor(file.path);
		const v = view;
		if (!v) {
			return;
		}
		v.dispatch({
			effects: editorConfigCompartment.reconfigure(editorConfigExtensions(ec)),
		});
	});

	// Light/dark flip. The chrome theme reads CSS variables for almost
	// everything, but CodeMirror also takes a `dark: boolean` flag at
	// theme-build time (used for built-in defaults like the drop cursor
	// color). We rebuild the theme + highlight bundle whenever the
	// *effective* theme (dark/light resolved from the user's choice +
	// system preference) flips. The HighlightStyle itself is static —
	// its CSS-variable colors re-resolve for free.
	$effect(() => {
		const mode = workspace.effectiveTheme;
		const v = view;
		if (!v) {
			return;
		}
		v.dispatch({
			effects: themeCompartment.reconfigure(moonEditorTheme(mode)),
		});
	});

	// LSP path facet: keep it in sync with the active buffer so hover
	// and completion adapters target the right file. We reconfigure on
	// every path change rather than tunnelling the path through CM
	// state so the facet value stays authoritative.
	$effect(() => {
		const v = view;
		if (!v) {
			return;
		}
		v.dispatch({
			effects: lspPathCompartment.reconfigure(filePathFacet.of(file.path)),
		});
	});

	// LSP diagnostics: push the latest list for this path into CM's
	// lint state. Untracked file types (no LSP wired up) never get
	// an entry in the map, so the fallback `[]` clears the gutter
	// — exactly what we want when switching from a TS file to a
	// markdown one.
	$effect(() => {
		const v = view;
		if (!v) {
			return;
		}
		const list = workspace.diagnostics.get(file.path) ?? [];
		applyDiagnostics(v, list);
	});

	// Pull focus into the editor whenever the workspace bumps `focusTick`.
	// That covers tab clicks, tree clicks (re-opening a closed file
	// included), and post-close fallback. Microtask-deferred so the click
	// that triggered the bump finishes settling its own focus first —
	// otherwise the browser sometimes hands focus back to the clicked
	// element after we call `view.focus()`.
	//
	// CRITICAL: only refocus on the *focused* side. Without this guard,
	// both editors in a split race to call `view.focus()`; whichever wins
	// makes its pane fire `focusin`, which sets `workspace.focusedSide`
	// back to that side — so clicking a tab on the unfocused pane would
	// snap focus right back to the original pane.
	$effect(() => {
		workspace.focusTick;
		if (workspace.focusedSide !== side) {
			return;
		}
		const v = view;
		if (!v) {
			return;
		}
		queueMicrotask(() => v.focus());
	});

	function baseExtensions() {
		const ec = workspace.editorConfigFor(file.path);
		return [
			lineNumbers(),
			highlightActiveLine(),
			highlightActiveLineGutter(),
			bracketMatching(),
			closeBrackets(),
			indentOnInput(),
			history(),
			highlightSelectionMatches(),
			// LSP diagnostics: the gutter extension paints severity
			// markers; `setDiagnostics` gets dispatched by the
			// reactive `$effect` below.
			...lspDiagnosticsExtension(),
			lspHoverExtension(),
			lspPathCompartment.of(filePathFacet.of(file.path)),
			// Autocompletion popover. `activateOnTyping: false` keeps
			// it off the typing path so we don't leak the built-in
			// identifier source; `override` routes explicit
			// invocations (Ctrl-Space) to the LSP source when a
			// server is wired up, and returns null otherwise so
			// CM falls back to its defaults (empty list).
			autocompletion({
				activateOnTyping: false,
				override: [lspCompletionSource],
			}),
			keymap.of([
				...closeBracketsKeymap,
				...defaultKeymap,
				...historyKeymap,
				...searchKeymap,
				...completionKeymap,
				indentWithTab,
			]),
			themeCompartment.of(moonEditorTheme(workspace.effectiveTheme)),
			languageCompartment.of([]),
			editorConfigCompartment.of(editorConfigExtensions(ec)),
			highlightTabs(),
			EditorView.updateListener.of((update) => {
				if (update.docChanged) {
					const text = update.state.doc.toString();
					if (currentPath !== null) {
						workspace.updateText(currentPath, text);
					}
				}
			}),
		];
	}

	function editorConfigExtensions(ec: EditorConfig) {
		// `indent_style = tab` → Tab inserts `\t`. `indent_style = space`
		// → Tab inserts `indent_size` spaces. CodeMirror's `indentMore`
		// (bound to Tab via `indentWithTab`) reads `indentUnit` for the
		// per-level width, and `tabSize` for visual rendering of `\t`.
		const unit = ec.indent_style === 'tab' ? '\t' : ' '.repeat(Math.max(1, ec.indent_size));
		return [EditorState.tabSize.of(Math.max(1, ec.tab_width)), indentUnit.of(unit)];
	}

	async function applyLanguage(path: string, text: string) {
		// `text` is consulted only as a shebang source for extension-less
		// scripts (e.g. `.husky/pre-commit`); we pass it explicitly rather
		// than reading from `view.state.doc` because at switch time the
		// doc still holds the previous file.
		const newlineIdx = text.indexOf('\n');
		const firstLine = newlineIdx === -1 ? text : text.slice(0, newlineIdx);
		const ext = await languageFor(path, firstLine);
		view?.dispatch({
			effects: languageCompartment.reconfigure(ext),
		});
	}
</script>

<div class="editor" bind:this={host}></div>

<style>
	.editor {
		flex: 1;
		min-width: 0;
		min-height: 0;
		overflow: hidden;
		display: flex;
	}
	.editor :global(.cm-editor) {
		flex: 1;
		min-width: 0;
		min-height: 0;
	}
	.editor :global(.cm-editor.cm-focused) {
		outline: none;
	}
</style>
