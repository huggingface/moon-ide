<script lang="ts">
	import { onMount } from 'svelte';
	import { Compartment, EditorSelection, EditorState, Prec, type Extension } from '@codemirror/state';
	import { EditorView, highlightActiveLine, highlightActiveLineGutter, keymap, lineNumbers } from '@codemirror/view';
	import { defaultKeymap, history, historyKeymap, indentWithTab } from '@codemirror/commands';
	import { highlightSelectionMatches, searchKeymap } from '@codemirror/search';
	import { bracketMatching, indentOnInput, indentUnit } from '@codemirror/language';
	import { autocompletion, closeBrackets, closeBracketsKeymap, completionKeymap } from '@codemirror/autocomplete';
	import { MergeView, diff as rawDiff, goToNextChunk, goToPreviousChunk } from '@codemirror/merge';
	import { ipc } from '../ipc';
	import { workspace, type OpenFile, type SplitSide } from '../state.svelte';
	import { highlightTabs } from '../editor/highlightTabs';
	import { languageFor } from '../editor/language';
	import { moonEditorTheme } from '../editor/theme';
	import { gitChangesExtension, headTextFacet, overviewMountFacet } from '../editor/gitChanges';
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
	// Right-pane HEAD facet for the change-bar gutter and the
	// overview-ruler marks. Keeps the diff view's right side
	// visually consistent with the regular editor — same gutter
	// glyphs, same clickable scrollbar strip — without rebuilding
	// the editor when HEAD shifts under us.
	const headB = new Compartment();
	// Soft-wrap compartments, one per side. The MergeView locks the
	// two scrollers in step, so flipping wrap on one side without
	// the other would wreck alignment between hunks.
	const wrapA = new Compartment();
	const wrapB = new Compartment();

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
			// Drop any selection snapshot the right pane published
			// while we were live — symmetric with `Editor.svelte`'s
			// teardown so the floating "Add to Coder" hint can't
			// outlive the surface that produced its selection.
			workspace.setActiveSelection(null);
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
			// Baseline-aware fetch: `'default'` reads the file at
			// the cached merge-base SHA; everything else falls
			// through to the regular HEAD blob.
			const mergeBase = workspace.defaultBranchMergeBase;
			const fetched =
				workspace.compareBaseline === 'default' && mergeBase !== null
					? await ipc.fs.gitRefContent(mergeBase, path)
					: await ipc.fs.gitHeadContent(path);
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
			wrapA.of(workspace.lineWrap ? EditorView.lineWrapping : []),
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

		// Per-line change gutter + clickable overview-ruler. Same
		// extension the regular editor uses, fed by the same
		// `headByPath` cache, so diff and editor share one
		// vocabulary for "where are the changes". `onGutterClick`
		// is omitted: clicking a marker while already in diff mode
		// would no-op anyway. Deleted buffers skip the gutter
		// because their right pane is empty.
		//
		// `overviewMountFacet` re-parents the strip from the inner
		// `.cm-editor` to **`.diff-host`** (a sibling of, not a
		// descendant of, `.cm-mergeView`). Two layout reasons stack:
		//
		//   1. The merge package forces `.cm-scroller` to
		//      `height: auto / overflow-y: visible` and scrolls on
		//      `.cm-mergeView`. Inside `.cm-editor` the overlay
		//      would render at doc height.
		//   2. `position: absolute` children of an `overflow: auto`
		//      ancestor still belong to the scrolling layer — they
		//      scroll with the content even though they're absolutely
		//      positioned. So just re-parenting to `.cm-mergeView`
		//      isn't enough either.
		//
		// `.diff-host` doesn't scroll and is the positioned ancestor;
		// `top:0; right:0; bottom:0` pins the strip to its right
		// edge, which lines up with `.cm-mergeView`'s scrollbar
		// because `.cm-mergeView` is `flex: 1` of `.diff-host`.
		const gitChangeExtensions: Extension[] = file.isDeleted
			? []
			: [gitChangesExtension(), headB.of(headTextFacet.of(head)), overviewMountFacet.of(() => host)];

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
			...gitChangeExtensions,
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
			wrapB.of(workspace.lineWrap ? EditorView.lineWrapping : []),
			...escapeBinding,
			EditorView.updateListener.of((update) => {
				if (update.docChanged) {
					const next = update.state.doc.toString();
					// Pipe edits through the same `updateText` path as
					// the regular editor — sets isDirty, lets the tab
					// strip render the dirty dot, and (because we share
					// the OpenFile buffer with the editor view) keeps
					// state coherent across the diff/edit toggle.
					workspace.updateText(path, next);
				}
				if (update.selectionSet) {
					// Mirror the regular editor's selection-publish
					// hook so `Ctrl+L` can attach a selection from the
					// diff view's right-hand pane to the coder.
					// Deleted buffers skip this — the right pane is
					// read-only and rendered empty for them, so any
					// selection there is meaningless.
					if (file.isDeleted) {
						return;
					}
					publishDiffSelection(update.state);
				}
			}),
			...(file.isDeleted ? [EditorState.readOnly.of(true), EditorView.editable.of(false)] : []),
		];

		merge = new MergeView({
			a: { doc: head, extensions: sharedLeft },
			b: { doc: rightText, extensions: rightExtensions },
			parent: host,
			gutter: true,
			highlightChanges: true,
			// Show the full file. Earlier we collapsed unchanged
			// regions behind a `… N unchanged lines` placeholder,
			// but with the change-bar gutter and the right-edge
			// overview-ruler in place the user already has a
			// "where are the changes" affordance — the placeholder
			// just got in the way of `Ctrl+F`, scrolling, and
			// reading code in context. Auto-scroll below jumps the
			// editor to the first chunk so opening a 3000-line
			// file with one edit at line 2500 still lands the
			// user at the change.
			revertControls: 'a-to-b',
			diffConfig: {
				// `MergeView` defaults to `presentableDiff`, which
				// runs a final `mergeAdjacent(changes, 3)` step that
				// fuses any two changes separated by fewer than 3
				// unchanged *characters* (not lines — the threshold
				// is character distance in both A and B).
				//
				// On its own that sounds harmless, but a single
				// `Change` object can already span many lines: when
				// you insert a multi-line block, the newlines live
				// inside `fromB..toB` of one `Change`. In a refactor
				// like `{ $set: {status, rejectionReason} }` →
				// a multi-line aggregation pipeline, the diff finds
				// short common substrings (`"status"`, `", "`) inside
				// the new text and emits the inserts on either side
				// as separate `Change`s only 1–2 chars apart. The
				// 3-char merge then fuses those, and because each
				// half was already multi-line, the merged `Change`
				// — and so the saturated `cm-changedText` — covers
				// several lines of mostly-new content with a few
				// matched substrings inlined. Visually that reads
				// as "the highlight jumped across unchanged text".
				//
				// The raw `diff` only does the gap-1 merge that
				// `normalize` runs anyway, so each highlighted span
				// is one contiguous insertion or replacement and any
				// matched substring (e.g. `"status"`) drops out of
				// the saturated highlight. The cost is losing the
				// word-boundary alignment `presentableDiff` does
				// before the merge step (a `foo` → `fooz` rename
				// highlights `o` → `oz` rather than the whole word),
				// which is a small readability regression — the
				// surrounding monospace + the per-line gutter still
				// make the change obvious. If the unaligned edges
				// become a problem we can re-introduce the word-
				// alignment loop without the 3-char merge step.
				override: rawDiff,
			},
		});

		const chunks = merge.chunks;
		if (chunks.length > 0 && !file.isDeleted) {
			const first = chunks[0];
			if (first) {
				const docLen = merge.b.state.doc.length;
				const pos = Math.min(first.fromB, docLen);
				// Defer the scroll into the next animation frame —
				// dispatching synchronously inside the MergeView
				// constructor's tail races with the library's own
				// initial layout and the scroll target lands at 0
				// instead of the chunk. One rAF after build is
				// enough for the inner `.cm-scroller` to settle.
				requestAnimationFrame(() => {
					if (token !== buildToken || !merge) {
						return;
					}
					merge.b.dispatch({
						selection: EditorSelection.cursor(pos),
						effects: EditorView.scrollIntoView(pos, { y: 'center' }),
					});
				});
			}
		}
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
		// Keep the right pane's `headTextFacet` aligned with the
		// left doc so the change-bar gutter and overview-ruler
		// repaint instead of pointing at stale HEAD content.
		// Skipped for deleted buffers — they don't carry the
		// extension.
		if (!file.isDeleted) {
			m.b.dispatch({ effects: headB.reconfigure(headTextFacet.of(head)) });
		}
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

	// Soft-wrap. Both sides of the merge get the same setting in the
	// same dispatch — flipping just one side would desync the
	// MergeView's gutter alignment between hunks.
	$effect(() => {
		const wrap = workspace.lineWrap;
		const m = merge;
		if (!m) {
			return;
		}
		const ext = wrap ? EditorView.lineWrapping : [];
		m.a.dispatch({ effects: wrapA.reconfigure(ext) });
		m.b.dispatch({ effects: wrapB.reconfigure(ext) });
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

	/**
	 * Mirror of `Editor.svelte`'s `publishSelection` for the diff
	 * view's right-hand (working-tree) editor. Same line-trimming
	 * heuristic — when the user's drag ends at a line's `from`,
	 * snap back so a `89-101` drag doesn't accidentally publish
	 * `89-102`. Empty selections clear the snapshot.
	 */
	function publishDiffSelection(state: EditorState) {
		const sel = state.selection.main;
		if (sel.empty) {
			workspace.setActiveSelection(null);
			return;
		}
		const fromLine = state.doc.lineAt(sel.from);
		const toLine = state.doc.lineAt(sel.to);
		const effectiveToLineNumber =
			sel.to === toLine.from && toLine.number > fromLine.number ? toLine.number - 1 : toLine.number;
		const text = state.doc.sliceString(sel.from, sel.to);
		workspace.setActiveSelection({
			path: file.path,
			startLine: fromLine.number,
			endLine: effectiveToLineNumber,
			text,
		});
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
		/* Positioning context for the re-parented git-overview
		   strip. The strip lives here as a sibling of `.cm-mergeView`
		   so it doesn't ride the merge view's scroll layer. */
		position: relative;
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
	/* Character-level change marker: the library default is a 2px
	 * bottom-edge gradient, which (a) reads as a loud underline on
	 * top of the already-tinted line and (b) collides visually with
	 * LSP lint underlines that use the same bottom-edge gesture.
	 * Swap for a soft same-hue background (GitHub-style inline diff
	 * highlight). Uses our palette via `color-mix` so theme flips
	 * track. `!important` is needed to beat the package's themed
	 * rules without a fragile selector-specificity arms race. */
	.diff-host :global(.cm-merge-b .cm-changedText) {
		background: color-mix(in srgb, var(--m-success) 22%, transparent) !important;
		border-radius: 2px;
	}
	.diff-host :global(.cm-merge-a .cm-changedText),
	.diff-host :global(.cm-deletedChunk .cm-deletedText) {
		background: color-mix(in srgb, var(--m-danger) 22%, transparent) !important;
		border-radius: 2px;
	}
	.diff-host :global(.cm-merge-b .cm-deletedText) {
		background: color-mix(in srgb, var(--m-danger) 22%, transparent) !important;
	}
</style>
