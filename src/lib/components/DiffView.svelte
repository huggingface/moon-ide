<script lang="ts">
	import { onMount } from 'svelte';
	import { Compartment, EditorSelection, EditorState, Prec, type Extension } from '@codemirror/state';
	import { EditorView, highlightActiveLine, highlightActiveLineGutter, keymap, lineNumbers } from '@codemirror/view';
	import {
		addCursorAbove,
		addCursorBelow,
		defaultKeymap,
		history,
		historyKeymap,
		indentWithTab,
	} from '@codemirror/commands';
	import { highlightSelectionMatches, searchKeymap } from '@codemirror/search';
	import { searchAsYouType } from '../editor/searchAsYouType';
	import { bracketMatching, foldGutter, indentOnInput, indentUnit } from '@codemirror/language';
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
		offsetForLspPosition,
	} from '../editor/lsp';
	import { diffPureChangeExtension } from '../editor/diffPureChange';
	import { diffGutterTintExtension } from '../editor/diffGutterTint';
	import { commentsForSide, reanchorComments, ReviewWiring } from '../editor/reviewComments';
	import { blameExtension, blameFacet } from '../editor/blame';
	import { lspGotoDefinitionExtension } from '../editor/lspGotoDefinition';
	import { lspOverviewExtension } from '../editor/lspOverview';
	import { lspRenameExtension } from '../editor/lspRename';
	import { lspLanguageFor } from '../editor/lspLanguage';
	import { EditorContextMenu } from '../editor/editorContextMenu';
	import { isReviewPath } from '../util/reviewPath';
	import { frontendLog } from '../logs.svelte';
	import type { EditorConfig } from '../protocol';

	// Mirror of `Editor.svelte`'s `logCtrlSpace`. Taps Ctrl+Space at
	// `Prec.high`, logs the breadcrumb into the `editor.completion`
	// diag-logs source, and returns `false` so the keystroke falls
	// through to `completionKeymap`'s canonical Ctrl-Space binding
	// (which calls `startCompletion`). Lives in DiffView so the
	// log fires regardless of which surface the user is in —
	// debugging "did Ctrl+Space land?" shouldn't require knowing
	// whether the buffer is in source or diff mode.
	function logCtrlSpace(): boolean {
		frontendLog('editor.completion', 'info', 'Ctrl+Space pressed');
		return false;
	}

	type Props = { file: OpenFile; side: SplitSide };
	let { file, side }: Props = $props();

	let host: HTMLDivElement;
	// `merge` is reactive so the `$effect`s below re-fire once
	// `buildMerge` (async — language load + head fetch) finally
	// assigns it. Without `$state`, the diagnostics / pending-jump
	// effects would run once at mount with `merge` undefined,
	// return early, and never re-run unless their *other*
	// dependencies (`workspace.diagnostics`, `pendingJumps`)
	// changed afterwards. Flipping into diff mode with already-
	// published diagnostics would then leave the right pane
	// blank — no squigglies, no overview-ruler markers — until
	// the next edit nudged the map. `$state` on this single
	// `let` is enough; the MergeView instance itself isn't
	// proxied (class instances pass through `$state` as-is in
	// Svelte 5), so method calls and internal state stay intact.
	let merge: MergeView | undefined = $state(undefined);
	// Cleanup for the horizontal-scroll sync wired up at the tail
	// of `buildMerge`. Cleared on teardown so we don't leak DOM
	// listeners across diff-view remounts.
	let detachHScrollSync: (() => void) | null = null;
	// Cleanup for the per-side sticky synthetic horizontal
	// scrollbars. Same lifecycle as the sync above — created in
	// `buildMerge` after the MergeView mounts, torn down on
	// teardown / rebuild.
	let detachStickyHbarA: (() => void) | null = null;
	let detachStickyHbarB: (() => void) | null = null;

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
	// Right-pane git-blame facet. Same in-place reconfigure pattern
	// as `Editor.svelte` so a late-arriving `workspace.blameByPath`
	// entry lights up the current-line annotation without
	// rebuilding the merge view. Left pane (HEAD blob) intentionally
	// doesn't get blame: those lines are pinned to whatever HEAD
	// says, and the per-line author is the same data we'd show on
	// the editor view of the same file — duplicating it on both
	// columns just adds visual noise.
	const blameB = new Compartment();
	// Review comments (Phase 5.7) on the right (working) pane. Same
	// gate and wiring shape as `Editor.svelte`; the left pane stays
	// comment-free here — base-side comments live in the Review tab,
	// where the full stacked-diff context makes them legible.
	const reviewCompartment = new Compartment();
	const reviewWiring = new ReviewWiring('working', {
		add: (args) => {
			workspace.addReviewComment({ path: file.path, ...args });
		},
		edit: (id, body) => workspace.editReviewComment(id, body),
		remove: (id) => workspace.deleteReviewComment(id),
		baselineRev: () =>
			workspace.compareBaseline === 'default' ? (workspace.defaultBranchMergeBase ?? 'HEAD') : 'HEAD',
	});
	const reviewComments = $derived(commentsForSide(workspace.reviewCommentsForPath(file.path), 'working'));
	const reviewEnabled = $derived(workspace.isReviewableBranch && !file.isDeleted);

	// Right-click menu on the editable right-hand (working-tree) pane —
	// same "Rename symbol" + "Copy GitHub link" actions as the regular
	// editor, shared via `EditorContextMenu`. Left-pane (HEAD) clicks
	// fall through to the platform menu.
	const editorMenu = new EditorContextMenu();

	function openDiffMenu(event: MouseEvent) {
		if (merge === undefined || file.isExternal || isReviewPath(file.path)) {
			return;
		}
		const rightDom = merge.b.dom;
		if (!(event.target instanceof Node) || !rightDom.contains(event.target)) {
			return;
		}
		// Deleted files have an empty, LSP-less right pane — no rename.
		const canRename = !file.isDeleted && lspLanguageFor(file.path) !== null;
		editorMenu.open(event, merge.b, file.path, { canRename });
	}

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
			detachHScrollSync?.();
			detachHScrollSync = null;
			detachStickyHbarA?.();
			detachStickyHbarA = null;
			detachStickyHbarB?.();
			detachStickyHbarB = null;
			// Snapshot the right pane's caret + scroll before
			// teardown so the matching `Editor.svelte` mount (or
			// a later return to diff mode) can pick up where the
			// user left off. Symmetric with `Editor.svelte`'s own
			// unmount snapshot — together they preserve the caret
			// across Ctrl+Shift+D toggles. History JSON is
			// deliberately not captured here: the right pane is
			// editable but the undo stack lives on the Editor
			// surface's lifecycle, and feeding a MergeView-flavoured
			// history JSON back into a regular EditorState would
			// at best be a no-op and at worst confuse `fromJSON`.
			const folder = workspace.activeFolderPath;
			if (merge && !file.isDeleted && folder !== null) {
				workspace.snapshotViewState(folder, file.path, {
					caretOffset: merge.b.state.selection.main.head,
					anchorOffset: merge.b.state.selection.main.anchor,
					scrollTop: merge.b.scrollDOM.scrollTop,
				});
			}
			reviewWiring.detach();
			editorMenu.dispose();
			merge?.destroy();
			merge = undefined;
			// Drop any selection snapshot the right pane published
			// while we were live — symmetric with `Editor.svelte`'s
			// teardown so the floating "Add to Coder" hint can't
			// outlive the surface that produced its selection.
			workspace.setActiveSelection(null);
		};
	});

	// Keep the review-comment bundle in sync with the gate and the
	// comment list (same full-reconfigure approach as `Editor.svelte`).
	$effect(() => {
		const enabled = reviewEnabled;
		const comments = reviewComments;
		const m = merge;
		if (!m) {
			return;
		}
		m.b.dispatch({
			effects: reviewCompartment.reconfigure(enabled ? reviewWiring.extension(comments) : []),
		});
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
		// keyboard focus. Deleted buffers get the binding too —
		// the EditorPane no longer force-routes them here, so
		// "explicitly opened diff (right-click View diff) → Esc"
		// is a real flip to the read-only Editor view of HEAD.
		const escapeBinding = [
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
			// Tint the line-number cell red on deleted / modified
			// lines so the chrome stays narrow (no dedicated
			// change-bar gutter — see `diffGutterTintExtension`
			// and the `gutter: false` pass to `MergeView` below).
			diffGutterTintExtension('a'),
			// See `diffPureChange.ts` — tags whole-chunk pure-delete
			// lines on A so the inner per-character red highlight
			// is stripped (the gutter and line tint already say
			// "this entire line is gone").
			diffPureChangeExtension,
			// Code folding mirrors the regular editor — both sides
			// of the diff fold independently. `MergeView` already
			// reacts to per-side `heightChanged` (via its update
			// listener calling `updateSpacers`) and re-emits block
			// spacers on whichever side is shorter at the start of
			// each unchanged region, so folding a function on one
			// side just rebalances the alignment. We skip the CM
			// `foldKeymap` for the same AltGr-layout reason as
			// the regular editor — see the comment on `foldGutter`
			// in `Editor.svelte` for the full rationale.
			foldGutter(),
			EditorState.readOnly.of(true),
			EditorView.editable.of(false),
			highlightSelectionMatches(),
			searchAsYouType(),
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
					// LSP overview ruler — same lane as the regular
					// editor, mounted into `.diff-host` via
					// `overviewMountFacet` (set below in
					// `gitChangeExtensions`).
					lspOverviewExtension,
					lspHoverExtension(),
					lspGotoDefinitionExtension({
						jumpTo: (target, position, folder) => workspace.jumpTo(target, position, side, folder),
						resolveExternalUri: (uri) => workspace.resolveExternalUri(uri),
						recordSourcePosition: (target, position) => {
							const folder = workspace.activeFolderPath;
							if (folder !== null) {
								workspace.pushClickNavigation(folder, target, position);
							}
						},
						flash: (msg) => workspace.flash(msg),
					}),
					// F2 rename on the editable right-hand pane — same
					// extension and applier the regular editor uses. The
					// docked rename panel mounts at the top of this pane;
					// edits route through `applyWorkspaceEdit` exactly as
					// in `Editor.svelte`.
					lspRenameExtension(),
					// `defaultKeymap: false` here mirrors `Editor.svelte`:
					// with the default on, `autocompletion()` installs
					// the upstream `completionKeymap` at `Prec.highest`
					// internally, which would shadow our `Prec.high`
					// Ctrl+Space breadcrumb tap below. We install
					// `completionKeymap` ourselves at `Prec.high` so the
					// tap fires first and falls through to the canonical
					// completion handlers.
					autocompletion({
						activateOnTyping: false,
						override: [lspCompletionSource],
						defaultKeymap: false,
					}),
					Prec.high(
						keymap.of([
							// Returns `false` so the keystroke
							// continues to the `completionKeymap`
							// block below (same `Prec.high`,
							// registered after us, so
							// within-precedence ordering hands
							// Ctrl-Space to it next) and
							// `startCompletion` fires there.
							{ key: 'Ctrl-Space', run: logCtrlSpace },
						]),
					),
					// `completionKeymap` at `Prec.high` so its
					// `ArrowDown` / `ArrowUp` / `Enter` / `Escape`
					// handlers beat the corresponding bindings in
					// `defaultKeymap` while the popup is open (each
					// handler returns `false` when the popup is
					// closed, so the default-precedence bindings still
					// own those keys for regular editing). Without
					// this lift, the same regression as `Editor.svelte`
					// hit before — popup nav lost to caret motion.
					Prec.high(keymap.of([...completionKeymap])),
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

		// Inline git-blame annotation, same as the regular editor.
		// Deleted buffers skip it: the right pane is empty and
		// read-only for them, so there's nothing to annotate.
		// Staleness while typing matches editor mode — lines below
		// an insertion show the wrong author until the next save
		// re-runs `git blame`.
		const blameExtensions: Extension[] = file.isDeleted
			? []
			: [blameExtension(), blameB.of(blameFacet.of(workspace.blameByPath.get(path) ?? null))];

		const rightExtensions: Extension[] = [
			lineNumbers(),
			// Tint the line-number cell green on added / modified
			// lines (mirrors the left pane's red on deleted /
			// modified). See `diffGutterTintExtension` and the
			// `gutter: false` pass to `MergeView` below for the
			// replacement of the package's built-in 3px gutter bar.
			diffGutterTintExtension('b'),
			// Same pure-add/delete tagging as the left pane — here it
			// strips the inner green highlight on whole-chunk
			// additions, since the line gutter + background already
			// communicate the change.
			diffPureChangeExtension,
			// See comment on the left pane's `foldGutter()` —
			// MergeView's spacer-based alignment absorbs per-side
			// fold height changes, so it's safe to enable folding
			// on the right too. No `foldKeymap` either (same
			// AZERTY-layout reason as the regular editor).
			foldGutter(),
			highlightActiveLine(),
			highlightActiveLineGutter(),
			bracketMatching(),
			closeBrackets(),
			indentOnInput(),
			history(),
			highlightSelectionMatches(),
			searchAsYouType(),
			highlightTabs(),
			...gitChangeExtensions,
			...blameExtensions,
			...lspExtensions,
			// Review comments: cards + hover "+" gutter + `Mod-Alt-c`,
			// only on reviewable branches (see `reviewEnabled`).
			reviewCompartment.of(reviewEnabled ? reviewWiring.extension(reviewComments) : []),
			keymap.of([
				// Alt+Left / Alt+Right ride the global handler in
				// `App.svelte` so they swallow consistently across
				// view kinds (no fallback to CM word-motion).
				// `completionKeymap` lives above at `Prec.high` —
				// don't re-spread it here, that would duplicate
				// handlers at default precedence where they'd lose
				// to `defaultKeymap` for the popup-open keys.
				//
				// "Next change" in the diff view is the merge
				// chunk nav (`goToNextChunk` / `goToPreviousChunk`)
				// bound further down on F7 / Shift-F7 — same keys
				// the regular editor uses for its git-change
				// gutter nav, so the gesture is consistent across
				// surfaces. Alt+Up / Alt+Down is left to CM's
				// native `moveLineUp` / `moveLineDown`.
				...closeBracketsKeymap,
				...defaultKeymap,
				...historyKeymap,
				...searchKeymap,
				indentWithTab,
				// Multi-cursor: Ctrl+Shift+Up/Down aliases the
				// `addCursorAbove` / `addCursorBelow` commands
				// that `defaultKeymap` binds to Ctrl+Alt+Up/Down,
				// for the VS Code / IntelliJ muscle memory.
				{ key: 'Mod-Shift-ArrowUp', run: addCursorAbove },
				{ key: 'Mod-Shift-ArrowDown', run: addCursorBelow },
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
					// Re-pin drifted review-comment hints (no-op unless
					// the buffer carries comments).
					for (const moved of reanchorComments(
						update.state,
						commentsForSide(workspace.reviewCommentsForPath(path), 'working'),
					)) {
						workspace.repinReviewComment(moved.id, moved.startLine, moved.endLine);
					}
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

		// Tear down any previous sync before rebuilding — `buildMerge`
		// is only called once per mount today, but keep this defensive
		// in case the build path grows a rebuild branch later.
		detachHScrollSync?.();
		detachHScrollSync = null;
		detachStickyHbarA?.();
		detachStickyHbarA = null;
		detachStickyHbarB?.();
		detachStickyHbarB = null;

		merge = new MergeView({
			a: { doc: head, extensions: sharedLeft },
			b: { doc: rightText, extensions: rightExtensions },
			parent: host,
			// The package's built-in `cm-changeGutter` (a 3px
			// coloured bar) is replaced by tinting the line-number
			// cell directly via `diffGutterTintExtension`. Keeping
			// the dedicated bar on top of the cell tint doubles
			// the same hue across two adjacent columns and reads
			// as visual noise without adding information.
			gutter: false,
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

		detachHScrollSync = wireHorizontalScrollSync(merge.a.scrollDOM, merge.b.scrollDOM);
		detachStickyHbarA = attachStickyHScrollbar(merge.a.scrollDOM, merge.a.dom);
		detachStickyHbarB = attachStickyHScrollbar(merge.b.scrollDOM, merge.b.dom);
		reviewWiring.attach(merge.b);

		// Caret restore order, highest precedence first:
		//
		//   1. Pending goto-def / search-hit jump — anything queued
		//      via `workspace.jumpTo` before the diff view mounted
		//      (Ctrl+Shift+F result, `Go to Definition` from the
		//      palette, coder tool-block click). The `pendingJumps`
		//      effect below applies it in a microtask; we just skip
		//      the snapshot / first-chunk restore so the later rAF
		//      doesn't clobber the jump's scroll target.
		//   2. View-state snapshot left by a previous mount of this
		//      buffer (Editor or DiffView). This is what makes a
		//      Ctrl+Shift+D toggle land back on the user's caret
		//      instead of resetting to the first chunk.
		//   3. First chunk's `fromB` — the legacy "open a 3000-line
		//      file and jump to the one edit at line 2500" gesture
		//      that fires on a clean first-time diff view.
		//   4. Nothing (deleted-file panes have a read-only empty
		//      right side; no chunks, no snapshot to restore).
		const folder = workspace.activeFolderPath;
		const pendingJump = folder !== null ? workspace.pendingJumps.get(`${folder}::${file.path}`) : undefined;
		const snapshot = folder !== null ? workspace.getViewState(folder, file.path) : null;
		const chunks = merge.chunks;
		if (pendingJump && folder !== null && !file.isDeleted) {
			// Apply the pending jump here (inside `buildMerge`)
			// rather than leaving it to the `pendingJumps`
			// `$effect` below for two reasons:
			//
			//   1. Same rAF deferral as the snapshot / first-chunk
			//      branches. Dispatching `scrollIntoView` in a
			//      microtask right after `new MergeView` runs the
			//      target before CM has computed layout, and the
			//      scroll lands at 0 — same race the first-chunk
			//      block already documents.
			//   2. Consuming the jump here means snapshot /
			//      first-chunk restore stays skipped, and the
			//      `$effect` consumer below (which is still wired
			//      for jumps that arrive *after* the diff view has
			//      mounted, e.g. Ctrl-click goto-def) has nothing
			//      to do this build.
			const docLen = merge.b.state.doc.length;
			const target = pendingJump;
			// Consume now so the `$effect` consumer below — which
			// re-runs when `merge` becomes defined and would
			// otherwise also fire a microtask-timed dispatch
			// (with the race the rAF below avoids) — sees an
			// empty `pendingJumps` for this buffer.
			workspace.consumePendingJump(folder, file.path);
			requestAnimationFrame(() => {
				if (token !== buildToken || !merge) {
					return;
				}
				const offset = Math.min(offsetForLspPosition(merge.b, target), docLen);
				merge.b.dispatch({
					selection: EditorSelection.cursor(offset),
					effects: EditorView.scrollIntoView(offset, { y: 'center' }),
				});
				merge.b.focus();
			});
		} else if (snapshot !== null && !file.isDeleted) {
			const docLen = merge.b.state.doc.length;
			// `caret` / `selAnchor` to dodge the outer-scope `head`
			// (HEAD blob text) shadowing.
			const caret = Math.min(snapshot.caretOffset, docLen);
			const selAnchor = Math.min(snapshot.anchorOffset, docLen);
			requestAnimationFrame(() => {
				if (token !== buildToken || !merge) {
					return;
				}
				merge.b.dispatch({
					selection: caret === selAnchor ? EditorSelection.cursor(caret) : EditorSelection.range(selAnchor, caret),
					effects: EditorView.scrollIntoView(caret, { y: 'center' }),
				});
				merge.b.scrollDOM.scrollTop = snapshot.scrollTop;
			});
		} else if (chunks.length > 0 && !file.isDeleted) {
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

	// Pending jumps targeted at this buffer's path land here exactly
	// like in the regular Editor view, so Ctrl-click goto-definition
	// from *inside* the diff view (or from any other surface that
	// targets a buffer we happen to have open in diff mode) moves
	// the right pane's caret to the LSP-returned line + character
	// instead of leaving it wherever the click happened. Without
	// this consumer the jump was queued but never applied — the
	// caret stayed at the clicked symbol and the user saw "nothing
	// happened" / "wrong line". Mirrors the consumer in
	// `Editor.svelte`; deleted buffers skip it because their right
	// pane is empty and read-only.
	//
	// `merge` is `$state` so this effect re-runs when `buildMerge`
	// finally assigns it — that's how a jump queued before the
	// MergeView finished building (e.g. a `Ctrl+Shift+F` hit that
	// opens a modified file fresh into diff mode) still gets
	// applied. `buildMerge` skips its own snapshot / first-chunk
	// scroll-restore when a pending jump exists for the buffer,
	// so the microtask dispatch below isn't fighting a later rAF
	// for the viewport.
	$effect(() => {
		const folder = workspace.activeFolderPath;
		if (folder === null || file.isDeleted) {
			return;
		}
		const key = `${folder}::${file.path}`;
		const pending = workspace.pendingJumps.get(key);
		if (!pending) {
			return;
		}
		const m = merge;
		if (!m) {
			return;
		}
		// Microtask defers the dispatch past any in-flight CM
		// state-rebuild (matches the timing rationale in
		// `Editor.svelte`'s consumer). Capturing `buildToken` /
		// rechecking `merge` would be defensive against a swap
		// mid-microtask, but the consumer is cheap enough that an
		// orphan dispatch into a torn-down view is just a no-op.
		queueMicrotask(() => {
			if (!merge) {
				return;
			}
			const offset = offsetForLspPosition(merge.b, pending);
			merge.b.dispatch({
				selection: EditorSelection.cursor(offset),
				effects: EditorView.scrollIntoView(offset, { y: 'center' }),
			});
			merge.b.focus();
			workspace.consumePendingJump(folder, file.path);
		});
	});

	// Blame updates from `workspace.blameByPath` (post-save refresh,
	// folder swap, etc). Mirrors `Editor.svelte`'s same-named effect:
	// reconfiguring the facet triggers the ViewPlugin's blame
	// branch on the next CM transaction without rebuilding state.
	// Skipped for deleted buffers — the right pane carries no
	// blame extension to reconfigure.
	$effect(() => {
		const blame = workspace.blameByPath.get(file.path) ?? null;
		const m = merge;
		if (!m || file.isDeleted) {
			return;
		}
		m.b.dispatch({
			effects: blameB.reconfigure(blameFacet.of(blame)),
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
		const perProducer = workspace.diagnosticsByProducer.get(file.path) ?? null;
		applyDiagnostics(m.b, perProducer);
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
	 * Keep the two side `.cm-scroller` elements horizontally
	 * aligned. `@codemirror/merge` already syncs vertical scroll
	 * (the outer `.cm-mergeView` drives both editors' Y axis) but
	 * each side's scroller owns its own X axis — so a long line
	 * on the left can scroll right while the right side stays
	 * pinned, which makes line-by-line comparison painful.
	 *
	 * We mirror `scrollLeft` either way: a scroll on `a` writes to
	 * `b` and vice-versa. Naively guarding with a time-windowed
	 * `syncing` flag (microtask / rAF) is racy when the two sides
	 * have different `scrollWidth`s: the wider side scrolls past
	 * the narrower side's max, the narrower side clamps, and the
	 * deferred `scroll` echo on the narrower side then yanks the
	 * wider side back to the clamped value. That's the "snaps
	 * back sometimes" behaviour reported on long-line diffs.
	 *
	 * Instead we identify echoes by *value*: when we mirror
	 * `from → to`, we record the value `to` will settle at (after
	 * the browser clamps it to `to`'s max scroll) as `expected[to]`.
	 * The next `scroll` event on `to` that matches `expected[to]`
	 * is consumed as the echo and cleared; anything else is a
	 * real user scroll and propagates. This is correct regardless
	 * of frame timing.
	 *
	 * Returns a cleanup that removes both listeners. Caller (the
	 * `onMount` teardown above) drops it on diff-view unmount.
	 */
	function wireHorizontalScrollSync(a: HTMLElement, b: HTMLElement): () => void {
		const expected = new WeakMap<HTMLElement, number>();
		const handle = (from: HTMLElement, to: HTMLElement) => {
			const pending = expected.get(from);
			if (pending !== undefined && from.scrollLeft === pending) {
				// Echo from our own programmatic write to `from`.
				expected.delete(from);
				return;
			}
			expected.delete(from);
			const toMax = Math.max(0, to.scrollWidth - to.clientWidth);
			const target = Math.min(from.scrollLeft, toMax);
			if (to.scrollLeft === target) {
				return;
			}
			expected.set(to, target);
			to.scrollLeft = target;
		};
		const onA = () => handle(a, b);
		const onB = () => handle(b, a);
		a.addEventListener('scroll', onA, { passive: true });
		b.addEventListener('scroll', onB, { passive: true });
		return () => {
			a.removeEventListener('scroll', onA);
			b.removeEventListener('scroll', onB);
		};
	}

	/**
	 * Per-side sticky synthetic horizontal scrollbar.
	 *
	 * `@codemirror/merge` lays the inner `.cm-scroller`s out at
	 * natural (doc) height so the outer `.cm-mergeView` can drive
	 * a single aligned vertical scroll across both sides. A side
	 * effect: each `.cm-scroller`'s native horizontal scrollbar
	 * lives at the bottom edge of that doc-tall element, which
	 * is far below the viewport for any non-trivial file. The
	 * user only sees the bar after scrolling all the way to the
	 * doc bottom, and CodeMirror's `.cm-panels-bottom` (Ctrl+F
	 * search panel) had the same problem until we relaxed
	 * `.cm-mergeViewEditor`'s overflow in CSS.
	 *
	 * The search panel already carries `position: sticky;
	 * bottom: 0` from CodeMirror's base theme, so relaxing
	 * overflow alone fixes it. The native horizontal scrollbar
	 * isn't a CSS element we can relocate, so we hide it
	 * (`scrollbar-width: none`) and render a synthetic bar
	 * sticky-bottom inside each column. The bar's inner spacer
	 * matches `scrollWidth`; bidirectional `scrollLeft` mirroring
	 * keeps the synthetic and the underlying scroller in lockstep.
	 *
	 * `--diff-hbar-h` is set on the column so the sibling search
	 * panel can sit just above the bar (see `.cm-panels-bottom`
	 * rule in the style block) — when no horizontal overflow
	 * exists we collapse the bar to zero so the panel sits flush
	 * with the mergeView's bottom edge.
	 */
	function attachStickyHScrollbar(scroller: HTMLElement, editorDom: HTMLElement): () => void {
		const column = editorDom.parentElement;
		if (column === null) {
			return () => {};
		}
		const bar = document.createElement('div');
		bar.className = 'diff-hscrollbar-sticky';
		const fill = document.createElement('div');
		fill.className = 'diff-hscrollbar-fill';
		bar.appendChild(fill);
		column.appendChild(bar);

		let syncing = false;
		const updateGeometry = () => {
			const sw = scroller.scrollWidth;
			const cw = scroller.clientWidth;
			fill.style.width = `${sw}px`;
			const hasOverflow = sw > cw + 1;
			bar.style.display = hasOverflow ? 'block' : 'none';
			// Reserve vertical space above the bar for the
			// (already-sticky) Ctrl+F panel so they don't overlap.
			column.style.setProperty('--diff-hbar-h', hasOverflow ? '14px' : '0px');
		};
		const onBarScroll = () => {
			if (syncing) {
				return;
			}
			if (bar.scrollLeft === scroller.scrollLeft) {
				return;
			}
			syncing = true;
			scroller.scrollLeft = bar.scrollLeft;
			requestAnimationFrame(() => {
				syncing = false;
			});
		};
		const onScrollerScroll = () => {
			if (syncing) {
				return;
			}
			if (scroller.scrollLeft === bar.scrollLeft) {
				return;
			}
			syncing = true;
			bar.scrollLeft = scroller.scrollLeft;
			requestAnimationFrame(() => {
				syncing = false;
			});
		};
		bar.addEventListener('scroll', onBarScroll, { passive: true });
		scroller.addEventListener('scroll', onScrollerScroll, { passive: true });

		const content = scroller.querySelector('.cm-content');
		const ro = new ResizeObserver(updateGeometry);
		ro.observe(scroller);
		if (content !== null) {
			ro.observe(content);
		}
		updateGeometry();

		return () => {
			bar.removeEventListener('scroll', onBarScroll);
			scroller.removeEventListener('scroll', onScrollerScroll);
			ro.disconnect();
			bar.remove();
			column.style.removeProperty('--diff-hbar-h');
		};
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
	<!-- svelte-ignore a11y_no_static_element_interactions -->
	<div class="diff-host" bind:this={host} oncontextmenu={openDiffMenu}></div>
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
	/* The merge package ships `.cm-mergeViewEditor { overflow: hidden }`,
	 * which makes each column its own scroll container (even though
	 * nothing actually scrolls there — overflow:hidden still
	 * registers as one). That hijacks `position: sticky` for
	 * descendants: CodeMirror's `.cm-panels.cm-panels-bottom` (the
	 * Ctrl+F search panel) and our synthetic horizontal scrollbar
	 * both want to stick to `.cm-mergeView`'s viewport, but they
	 * end up attaching to this hidden box and sit at the column's
	 * natural bottom (= bottom of the doc-tall editor). Relaxing
	 * the overflow lets the sticky chain walk past the column up
	 * to `.cm-mergeView`'s overflow-y:auto, which is the actual
	 * scroll container. Horizontal clipping is still done by the
	 * inner `.cm-scroller` (overflow-x: auto), so we don't get any
	 * visual leak from removing the column's clip. */
	.diff-host :global(.cm-mergeViewEditor) {
		min-width: 0;
		overflow: visible;
	}
	/* CodeMirror sets `position: sticky; bottom: 0` on `.cm-panels`
	 * via inline style (see `panels.syncDOM` in @codemirror/view).
	 * Override the inline `0` to make room for our sticky synthetic
	 * horizontal scrollbar below so they don't stack at the same
	 * viewport row. `--diff-hbar-h` is set by `attachStickyHScrollbar`
	 * — 14px while a horizontal scrollbar is needed, 0 otherwise.
	 * `!important` is required to beat the inline style. */
	.diff-host :global(.cm-panels.cm-panels-bottom) {
		bottom: var(--diff-hbar-h, 0px) !important;
	}
	/* Hide the native horizontal scrollbar — the synthetic
	 * sticky bar below replaces it at viewport bottom. Mouse-
	 * wheel / touch horizontal scrolling still works on the
	 * scroller; only the chrome moves. */
	.diff-host :global(.cm-scroller) {
		scrollbar-width: none;
	}
	.diff-host :global(.cm-scroller::-webkit-scrollbar) {
		display: none;
	}
	/* Sticky synthetic horizontal scrollbar. Lives inside
	 * `.cm-mergeViewEditor` (sibling of `.cm-editor`); because
	 * the column is now overflow:visible (above), sticky here
	 * attaches to `.cm-mergeView` and pins to its viewport
	 * bottom while the user scrolls. The inner `.diff-hscrollbar-fill`
	 * is sized to match the underlying `.cm-scroller`'s
	 * `scrollWidth` so the synthetic bar's thumb tracks the
	 * actual content. */
	.diff-host :global(.diff-hscrollbar-sticky) {
		position: sticky;
		bottom: 0;
		height: 14px;
		overflow-x: auto;
		overflow-y: hidden;
		z-index: 4;
		background: var(--m-bg);
	}
	.diff-host :global(.diff-hscrollbar-fill) {
		height: 1px;
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
	/* Pure-added / pure-deleted lines: the line-level change is
	 * already conveyed by the gutter bar (`.cm-changedLineGutter`
	 * — green on B for additions, red on A for deletions) and the
	 * subtle line-background tint. Layering the per-character
	 * highlight (`.cm-changedText`) on top doubles up the same hue
	 * across the entire line and reads as saturated noise without
	 * adding information. The line decoration `.cm-moon-pure-change`
	 * is added by `diffPureChange.ts` on any line whose entire
	 * content is part of a `Change` span — covering whole-chunk
	 * pure adds/deletes *and* the all-new / all-removed lines
	 * sitting inside an otherwise-modified chunk (e.g. a block of
	 * inserted comment lines between two unchanged anchors). Lines
	 * with surviving common substrings keep their per-character
	 * markers since the substring-vs-surrounding-text distinction
	 * is useful there. */
	.diff-host :global(.cm-moon-pure-change .cm-changedText) {
		background: transparent !important;
	}
</style>
