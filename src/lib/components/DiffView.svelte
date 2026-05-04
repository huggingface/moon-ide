<script lang="ts">
	import { onMount } from 'svelte';
	import { Compartment, EditorState, Prec, type Extension } from '@codemirror/state';
	import { EditorView, highlightActiveLine, highlightActiveLineGutter, keymap, lineNumbers } from '@codemirror/view';
	import { defaultKeymap, history, historyKeymap, indentWithTab } from '@codemirror/commands';
	import { highlightSelectionMatches, searchKeymap } from '@codemirror/search';
	import { bracketMatching, indentOnInput, indentUnit } from '@codemirror/language';
	import { autocompletion, closeBrackets, closeBracketsKeymap, completionKeymap } from '@codemirror/autocomplete';
	import { MergeView, goToNextChunk, goToPreviousChunk } from '@codemirror/merge';
	import { ipc } from '../ipc';
	import { workspace, type OpenFile, type SplitSide } from '../state.svelte';
	import { highlightTabs } from '../editor/highlightTabs';
	import { languageFor } from '../editor/language';
	import { moonEditorTheme } from '../editor/theme';
	import {
		applyDiagnostics,
		filePathFacet,
		lspCompletionSource,
		lspDiagnosticsExtension,
		lspHoverExtension,
	} from '../editor/lsp';
	import { lspGotoDefinitionExtension } from '../editor/lspGotoDefinition';
	import type { EditorConfig } from '../protocol';

	type Props = { file: OpenFile; side: SplitSide };
	let { file, side }: Props = $props();

	let host: HTMLDivElement;
	let merge: MergeView | undefined;

	// Compartments are per-side: each `EditorView` inside the
	// MergeView owns its own EditorState and can't share a Compartment
	// with the other. Mirror everything that flips at runtime —
	// theme, language, editorconfig — so we never have to rebuild
	// the merge view to react to a setting change.
	const langA = new Compartment();
	const langB = new Compartment();
	const themeA = new Compartment();
	const themeB = new Compartment();
	const ecA = new Compartment();
	const ecB = new Compartment();

	// In single-tab model, `file.path` is stable for the lifetime of
	// this DiffView instance — the EditorPane swaps Editor ↔ DiffView
	// based on `diffModeFor(path)`, which remounts DiffView on each
	// flip. So we build once in `onMount` and don't worry about
	// path swaps inside the view.
	let buildToken = 0;
	// Cached HEAD content currently rendered in the left side. Lets
	// us short-circuit no-op dispatches when the headByPath cache
	// fires reactively without the value actually changing.
	let currentHead: string | null = null;

	onMount(() => {
		void buildMerge();
		return () => {
			// Bump the build token so any in-flight `buildMerge`
			// resolves into a no-op instead of attaching to a
			// destroyed host node.
			buildToken++;
			merge?.destroy();
			merge = undefined;
		};
	});

	async function buildMerge() {
		const token = ++buildToken;
		const path = file.path;
		// Editorconfig + language load are async (the latter dynamic-
		// imports the grammar). Resolve both before constructing CM
		// state so we don't paint with the wrong settings for a frame.
		await workspace.ensureEditorConfig(path);
		const ec = workspace.editorConfigFor(path);
		const text = file.text;
		const newlineIdx = text.indexOf('\n');
		const firstLine = newlineIdx === -1 ? text : text.slice(0, newlineIdx);
		const lang = await languageFor(path, firstLine);

		// HEAD: prefer the warm cache (the gutter extension and
		// `setActive` keep it loaded for any open file). For deleted
		// buffers the HEAD content was captured at open time and
		// lives on `file.text` itself — but those render with empty
		// right side, so we still pull HEAD via the cache for the
		// left side and let the caller (EditorPane) decide.
		const cached = workspace.headByPath.get(path);
		let head: string;
		if (cached !== undefined) {
			head = cached ?? '';
		} else if (file.isDeleted) {
			head = file.text;
		} else {
			const fetched = await ipc.fs.gitHeadContent(path);
			if (token !== buildToken) {
				return;
			}
			head = fetched ?? '';
		}

		if (token !== buildToken) {
			return;
		}

		// Deleted buffers have no working tree to edit, so right
		// side renders empty and is read-only. Everything else gets
		// the live `file.text` and the same edit affordances as
		// the regular editor.
		const rightText = file.isDeleted ? '' : text;

		currentHead = head;

		// `Escape` flips the buffer back to the editor view. Wrapped
		// in `Prec.low` so CM's built-in Escape bindings (close the
		// search panel, dismiss an autocompletion popover) take
		// priority — only a "plain" Escape with no panel / popup
		// open trickles down to us. Bound on both sides so the
		// gesture works regardless of which pane currently has
		// keyboard focus. Skipped entirely for deleted buffers
		// because there's no editor mode to flip *to*.
		const escapeBinding = file.isDeleted
			? []
			: [
					Prec.low(
						keymap.of([
							{
								key: 'Escape',
								run: () => {
									workspace.setDiffMode(file.path, false);
									return true;
								},
							},
						]),
					),
				];

		const sharedLeft: Extension[] = [
			lineNumbers(),
			EditorState.readOnly.of(true),
			EditorView.editable.of(false),
			highlightSelectionMatches(),
			highlightTabs(),
			themeA.of(moonEditorTheme(workspace.effectiveTheme)),
			langA.of(lang),
			ecA.of(editorConfigExtensions(ec)),
			...escapeBinding,
		];

		// LSP wiring: the buffer's `didOpen` was already issued by
		// `workspace.openFile`, and Editor + DiffView are mutually
		// exclusive for any given path (EditorPane mounts one or
		// the other based on `diffModeFor`), so wiring the same
		// hover / completion / goto-def adapters into the
		// MergeView's right-hand editor doesn't risk a duplicate
		// `didOpen`. Edits route through `workspace.updateText` →
		// `lspScheduleUpdate` exactly like the editor view, and
		// the diagnostics list re-renders below via `applyDiagnostics`.
		// Deleted buffers skip the whole stack: there's no live
		// LSP open for a path that isn't on disk.
		const lspExtensions: Extension[] = file.isDeleted
			? []
			: [
					filePathFacet.of(path),
					...lspDiagnosticsExtension(),
					lspHoverExtension(),
					lspGotoDefinitionExtension({
						jumpTo: (target, position, folder) => workspace.jumpTo(target, position, side, folder),
						resolveExternalUri: (uri) => workspace.resolveExternalUri(uri),
						flash: (msg) => workspace.flash(msg),
					}),
					autocompletion({
						activateOnTyping: false,
						override: [lspCompletionSource],
					}),
				];

		const rightExtensions: Extension[] = [
			lineNumbers(),
			highlightActiveLine(),
			highlightActiveLineGutter(),
			bracketMatching(),
			closeBrackets(),
			indentOnInput(),
			history(),
			highlightSelectionMatches(),
			highlightTabs(),
			...lspExtensions,
			keymap.of([
				// Alt+Left / Alt+Right ride the global handler in
				// `App.svelte` so they swallow consistently across
				// view kinds (no fallback to CM word-motion).
				...closeBracketsKeymap,
				...defaultKeymap,
				...historyKeymap,
				...searchKeymap,
				...completionKeymap,
				indentWithTab,
				// F7 / Shift-F7 mirror the CodeMirror reference
				// merge example. Quick way to hop between hunks
				// without leaving the keyboard.
				{ key: 'F7', run: goToNextChunk },
				{ key: 'Shift-F7', run: goToPreviousChunk },
			]),
			themeB.of(moonEditorTheme(workspace.effectiveTheme)),
			langB.of(lang),
			ecB.of(editorConfigExtensions(ec)),
			...escapeBinding,
			EditorView.updateListener.of((update) => {
				if (!update.docChanged) {
					return;
				}
				const next = update.state.doc.toString();
				// Pipe edits through the same `updateText` path as
				// the regular editor — sets isDirty, lets the tab
				// strip render the dirty dot, and (because we share
				// the OpenFile buffer with the editor view) keeps
				// state coherent across the diff/edit toggle.
				workspace.updateText(path, next);
			}),
			...(file.isDeleted ? [EditorState.readOnly.of(true), EditorView.editable.of(false)] : []),
		];

		merge = new MergeView({
			a: { doc: head, extensions: sharedLeft },
			b: { doc: rightText, extensions: rightExtensions },
			parent: host,
			gutter: true,
			highlightChanges: true,
			collapseUnchanged: { margin: 3, minSize: 4 },
			revertControls: 'a-to-b',
		});
	}

	// External text edits to the right-side buffer (e.g. save-time
	// pipeline rewrite, or the Editor view editing the same buffer
	// while we're hidden) need to reach the MergeView's right editor
	// without rebuilding state. Guard on equality so our own
	// updateListener-driven dispatch doesn't ping-pong.
	$effect(() => {
		const text = file.text;
		const m = merge;
		if (!m) {
			return;
		}
		const current = m.b.state.doc.toString();
		if (current === text) {
			return;
		}
		m.b.dispatch({
			changes: { from: 0, to: m.b.state.doc.length, insert: text },
		});
	});

	// HEAD updates from `workspace.headByPath` (e.g. fs watcher fired
	// after `git commit` / `git checkout`). Keep `currentHead` in
	// lockstep with the dispatched doc so the next reactive run
	// short-circuits when nothing actually changed.
	$effect(() => {
		const cached = workspace.headByPath.get(file.path);
		const m = merge;
		if (!m || cached === undefined) {
			return;
		}
		const head = cached ?? '';
		if (currentHead === head) {
			return;
		}
		currentHead = head;
		m.a.dispatch({
			changes: { from: 0, to: m.a.state.doc.length, insert: head },
		});
	});

	$effect(() => {
		const mode = workspace.effectiveTheme;
		const m = merge;
		if (!m) {
			return;
		}
		m.a.dispatch({ effects: themeA.reconfigure(moonEditorTheme(mode)) });
		m.b.dispatch({ effects: themeB.reconfigure(moonEditorTheme(mode)) });
	});

	// LSP diagnostics: push the latest list for this path into the
	// right-side editor's lint state. Mirror of `Editor.svelte`'s
	// effect; the same backend cache (`workspace.diagnostics`) feeds
	// both views so flipping diff ↔ source carries the squigglies
	// across without re-fetching.
	$effect(() => {
		const m = merge;
		if (!m || file.isDeleted) {
			return;
		}
		const list = workspace.diagnostics.get(file.path) ?? [];
		applyDiagnostics(m.b, list);
	});

	$effect(() => {
		const ec = workspace.editorConfigFor(file.path);
		const m = merge;
		if (!m) {
			return;
		}
		m.a.dispatch({ effects: ecA.reconfigure(editorConfigExtensions(ec)) });
		m.b.dispatch({ effects: ecB.reconfigure(editorConfigExtensions(ec)) });
	});

	// Pull keyboard focus into the right-side editor (the editable
	// one) whenever the workspace bumps focusTick for our side.
	// Mirrors `Editor.svelte`'s pattern; the microtask defer lets
	// the click that triggered the bump finish settling first so
	// the browser doesn't hand focus back to the original target.
	$effect(() => {
		workspace.focusTick;
		if (workspace.focusedSide !== side) {
			return;
		}
		const m = merge;
		if (!m) {
			return;
		}
		queueMicrotask(() => m.b.focus());
	});

	function editorConfigExtensions(ec: EditorConfig): Extension {
		const unit = ec.indent_style === 'tab' ? '\t' : ' '.repeat(Math.max(1, ec.indent_size));
		return [EditorState.tabSize.of(Math.max(1, ec.tab_width)), indentUnit.of(unit)];
	}
</script>

<div class="diff-view">
	<div class="diff-host" bind:this={host}></div>
</div>

<style>
	.diff-view {
		flex: 1;
		min-width: 0;
		min-height: 0;
		display: flex;
		flex-direction: column;
		background: var(--m-bg);
		color: var(--m-fg);
		overflow: hidden;
	}
	.diff-host {
		flex: 1;
		min-width: 0;
		min-height: 0;
		display: flex;
		overflow: hidden;
	}
	/* `@codemirror/merge` ships its own layout: the outer `.cm-mergeView`
	 * is `overflow-y: auto`, the two `.cm-mergeViewEditor` columns are
	 * `flex: 1 0; overflow: hidden`, and inner `.cm-scroller`s render at
	 * natural height so the outer container drives a single, aligned
	 * scrollbar across both sides. We just need to bound the outer
	 * height — the rest of the chain is already correct. Don't set
	 * `display` or `flex-direction` here, that clobbers the package
	 * defaults and silently kills scrolling. */
	.diff-host :global(.cm-mergeView) {
		flex: 1;
		min-width: 0;
		min-height: 0;
	}
	.diff-host :global(.cm-editor.cm-focused) {
		outline: none;
	}
	.diff-host :global(.cm-mergeViewEditor) {
		min-width: 0;
	}
</style>
