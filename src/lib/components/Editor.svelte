<script lang="ts">
	import { onMount } from 'svelte';
	import { EditorState, Compartment } from '@codemirror/state';
	import { EditorView, highlightActiveLine, highlightActiveLineGutter, keymap, lineNumbers } from '@codemirror/view';
	import { highlightTabs } from '../editor/highlightTabs';
	import { defaultKeymap, history, historyKeymap, indentWithTab } from '@codemirror/commands';
	import { searchKeymap, highlightSelectionMatches } from '@codemirror/search';
	import {
		bracketMatching,
		indentOnInput,
		syntaxHighlighting,
		defaultHighlightStyle,
		indentUnit,
	} from '@codemirror/language';
	import { workspace, type OpenFile, type SplitSide } from '../state.svelte';
	import { languageFor } from '../editor/language';
	import { moonTheme } from '../editor/theme';

	type Props = { file: OpenFile; side: SplitSide };
	let { file, side }: Props = $props();

	let host: HTMLDivElement;
	let view: EditorView | undefined;
	const languageCompartment = new Compartment();

	// Editor defaults are hardcoded until Phase 1.5 wires `.editorconfig`.
	// House style is tabs at width 2 with tab markers visible. There is
	// deliberately no `settings.json` knob for these — see ADR 0006.
	const TAB_SIZE = 2;
	const INDENT_UNIT = '\t';

	// Each Editor instance owns one CM view that we re-target as the active file changes.
	// We track the path the view currently holds so we know when to swap state.
	let currentPath: string | null = null;

	onMount(() => {
		const state = EditorState.create({
			doc: file.text,
			extensions: baseExtensions(),
		});
		view = new EditorView({ state, parent: host });
		currentPath = file.path;
		void applyLanguage(file.path);
		return () => {
			view?.destroy();
			view = undefined;
		};
	});

	$effect(() => {
		const v = view;
		if (!v) {
			return;
		}
		if (file.path !== currentPath) {
			const next = EditorState.create({
				doc: file.text,
				extensions: baseExtensions(),
			});
			v.setState(next);
			currentPath = file.path;
			void applyLanguage(file.path);
			return;
		}
		// Same path, but the in-memory text may differ if state was mutated externally.
		if (file.text !== v.state.doc.toString()) {
			v.dispatch({
				changes: { from: 0, to: v.state.doc.length, insert: file.text },
			});
		}
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
		return [
			lineNumbers(),
			highlightActiveLine(),
			highlightActiveLineGutter(),
			bracketMatching(),
			indentOnInput(),
			history(),
			highlightSelectionMatches(),
			syntaxHighlighting(defaultHighlightStyle, { fallback: true }),
			keymap.of([...defaultKeymap, ...historyKeymap, ...searchKeymap, indentWithTab]),
			moonTheme,
			languageCompartment.of([]),
			EditorState.tabSize.of(TAB_SIZE),
			indentUnit.of(INDENT_UNIT),
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

	async function applyLanguage(path: string) {
		const ext = await languageFor(path);
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
