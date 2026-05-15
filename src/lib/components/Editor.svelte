<script lang="ts">
	import { onMount } from 'svelte';
	import { diffChars } from 'diff';
	import { EditorState, Compartment, EditorSelection, Prec, Transaction } from '@codemirror/state';
	import { EditorView, highlightActiveLine, highlightActiveLineGutter, keymap, lineNumbers } from '@codemirror/view';
	import { highlightTabs } from '../editor/highlightTabs';
	import { defaultKeymap, history, historyField, historyKeymap, indentWithTab } from '@codemirror/commands';
	import { searchKeymap, highlightSelectionMatches } from '@codemirror/search';
	import { bracketMatching, foldGutter, indentOnInput, indentUnit } from '@codemirror/language';
	import { autocompletion, closeBrackets, closeBracketsKeymap, completionKeymap } from '@codemirror/autocomplete';
	import {
		applyDiagnostics,
		filePathFacet,
		lspCompletionSource,
		lspDiagnosticsExtension,
		lspHoverExtension,
		offsetForLspPosition,
	} from '../editor/lsp';
	import { applyAutocompleteFromEditorView } from '../editor/autocompleteApply';
	import { frontendLog } from '../logs.svelte';
	import { lspGotoDefinitionExtension } from '../editor/lspGotoDefinition';
	import { lspOverviewExtension } from '../editor/lspOverview';
	import { lspRenameExtension } from '../editor/lspRename';
	import { blameExtension, blameFacet } from '../editor/blame';
	import { gitChangesExtension, headTextFacet } from '../editor/gitChanges';
	import { workspace, type OpenFile, type SplitSide } from '../state.svelte';
	import { languageFor } from '../editor/language';
	import { moonEditorTheme } from '../editor/theme';
	import { type EditorConfig, type LspPosition } from '../protocol';

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
	// Current git blame data, reconfigured in-place when the
	// workspace cache updates for this path. Kept in its own
	// compartment (rather than rebuilt into baseExtensions) so a
	// late-arriving blame response doesn't rebuild editor state —
	// reconfiguring a facet is a zero-cost transaction.
	const blameCompartment = new Compartment();
	// `HEAD` blob for the current buffer, feeding the git-changes
	// gutter. Same zero-rebuild reconfigure pattern as blame — late-
	// arriving `git show HEAD:<path>` responses just flip the facet
	// value and the StateField re-runs its line diff on the next
	// transaction.
	const headCompartment = new Compartment();
	// Soft-wrap toggle. Holds either `EditorView.lineWrapping` (when
	// `workspace.lineWrap` is on) or an empty extension array (off,
	// the default). A single window-global toggle drives every
	// editor pane through this compartment.
	const lineWrapCompartment = new Compartment();

	// Each Editor instance owns one CM view that we re-target as the active file changes.
	// We track the path the view currently holds so we know when to swap state.
	let currentPath: string | null = null;
	let lastHandledAutocompleteEditorTick = 0;

	function runAutocompleteFromShortcut(editorView: EditorView): boolean {
		frontendLog('editor.completion', 'debug', 'Ctrl+T pressed → applying next-edit autocomplete');
		void applyAutocompleteFromEditorView(editorView);
		return true;
	}

	// Ctrl+Space breadcrumb for the diag-logs panel. We tap the
	// key at `Prec.high`, log, and return `false` so the keystroke
	// falls through to `completionKeymap`'s canonical Ctrl-Space
	// binding (which calls `startCompletion`). We deliberately
	// don't claim the key here: that duplicates the side effect
	// for no reason, and — more subtly — would route the keystroke
	// through a different code path than every other completion
	// trigger, which made it hard to reason about which keymap
	// owned what. Returning `false` also keeps the binding
	// composable: any future second listener at higher precedence
	// can intercept Ctrl-Space without us having to coordinate.
	function logCtrlSpace(): boolean {
		frontendLog('editor.completion', 'info', 'Ctrl+Space pressed');
		return false;
	}

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
			// Clear any selection snapshot tied to this view so a
			// re-mount (HMR, file-tab close) doesn't leave the
			// "Add to Coder" hint hovering over a dead path.
			workspace.setActiveSelection(null);
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
			// Tab swap drops the previous file's selection
			// snapshot — Ctrl+L should never attach a selection
			// from a tab the user just left.
			workspace.setActiveSelection(null);
			// Capture the outgoing tab's caret + scroll + undo
			// history so coming back to it lands the user where
			// they left off *and* Ctrl+Z still walks through the
			// pre-switch edits. `setState` below replaces the
			// view's state wholesale, which is why this has to
			// happen *before* the swap. `renamed` falls through
			// because that path doesn't `setState` — the doc is
			// patched in place and the live selection + history
			// survive on their own.
			const folder = workspace.activeFolderPath;
			if (!renamed && currentPath !== null && folder !== null) {
				// `toJSON({ history })` returns `{ doc, selection,
				// history }`. We pass it whole into the snapshot —
				// the restore path only consumes the `history`
				// slot (the doc is re-sourced from `file.text`,
				// the cursor / scroll restore lives in the
				// dedicated fields below) but the JSON shape is
				// what CM's `fromJSON` reader expects.
				workspace.snapshotViewState(folder, currentPath, {
					caretOffset: v.state.selection.main.head,
					anchorOffset: v.state.selection.main.anchor,
					scrollTop: v.scrollDOM.scrollTop,
					historyJson: v.state.toJSON({ history: historyField }),
				});
			}
			currentPath = file.path;
			void workspace.ensureEditorConfig(file.path);
			void applyLanguage(file.path, file.text);
			if (renamed) {
				// Pipeline may have rewritten the bytes; sync the doc
				// without rebuilding state.
				syncDocText(v, file.text);
				return;
			}
			// Build the fresh state. When a snapshot with a
			// preserved history exists we route through
			// `fromJSON` so CM's undo stack reattaches; otherwise
			// `EditorState.create` is fine. We always override
			// `doc` with the workspace's authoritative text in
			// case it changed externally (F2 rename, format-on-
			// save in a sibling tab, coder writes) — the history
			// deltas may then refer to offsets that no longer
			// exist, but CM clamps internally and the worst case
			// is an undo that skips an external mutation rather
			// than corrupting the buffer.
			const snapshot = folder !== null ? workspace.getViewState(folder, file.path) : null;
			const next = snapshot?.historyJson
				? buildStateWithHistory(file.text, snapshot.historyJson)
				: EditorState.create({
						doc: file.text,
						extensions: baseExtensions(),
					});
			v.setState(next);
			// Restore the incoming tab's caret + scroll. A fresh
			// open (no prior snapshot) leaves the cursor at offset
			// 0, matching what `EditorState.create` already gives
			// us — no-op for first-time views. The scroll restore
			// is microtask-deferred so CodeMirror has flushed its
			// post-`setState` measurement pass and `scrollDOM`'s
			// `scrollHeight` reflects the new doc; setting
			// `scrollTop` before that lands at 0 silently. A
			// downstream pending-jump (Ctrl/Cmd-click goto-def
			// arrived at this path) still wins because its effect
			// queues *another* dispatch in a later microtask that
			// overwrites the restore — deliberate ordering, see
			// the pending-jump effect's comment for the same
			// reasoning.
			if (snapshot !== null) {
				const docLen = v.state.doc.length;
				const head = Math.min(snapshot.caretOffset, docLen);
				const anchor = Math.min(snapshot.anchorOffset, docLen);
				v.dispatch({
					selection: head === anchor ? EditorSelection.cursor(head) : EditorSelection.range(anchor, head),
				});
				queueMicrotask(() => {
					v.scrollDOM.scrollTop = snapshot.scrollTop;
				});
			}
			return;
		}
		// Same path, but the in-memory text may differ if state was mutated externally.
		syncDocText(v, file.text);
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

	// Soft-wrap toggle. Reading `workspace.lineWrap` here registers
	// the dependency so `Alt+Z` (or the palette command) flips every
	// mounted Editor at once, including any panes that mount later.
	$effect(() => {
		const wrap = workspace.lineWrap;
		const v = view;
		if (!v) {
			return;
		}
		v.dispatch({
			effects: lineWrapCompartment.reconfigure(wrap ? EditorView.lineWrapping : []),
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

	// Blame data. Reconfiguring the facet is what makes the
	// ViewPlugin's `update.startState.facet(blameFacet) !==
	// update.state.facet(blameFacet)` branch fire — which is how
	// the current-line annotation appears / refreshes after the
	// async IPC resolves. `null` is the "no blame" signal and
	// yields an empty decoration set.
	$effect(() => {
		const v = view;
		if (!v) {
			return;
		}
		const blame = workspace.blameByPath.get(file.path) ?? null;
		v.dispatch({
			effects: blameCompartment.reconfigure(blameFacet.of(blame)),
		});
	});

	// `HEAD` content for the git-changes gutter. `undefined` in the
	// map means "not asked yet"; `null` means "asked, no HEAD
	// available" (untracked / outside-repo / binary). Both collapse
	// to `null` on the facet, which the extension treats as "no
	// gutter paints today".
	$effect(() => {
		const v = view;
		if (!v) {
			return;
		}
		const head = workspace.headByPath.get(file.path) ?? null;
		v.dispatch({
			effects: headCompartment.reconfigure(headTextFacet.of(head)),
		});
	});

	// Consume a pending jump (Ctrl/Cmd-click on an identifier lands
	// here): `workspace.jumpTo` stashed the target position, and the
	// Editor that ends up owning `file.path` applies the selection
	// change + scrolls on its first render for this path. The jump
	// is one-shot — once consumed, the entry is dropped so the
	// caret doesn't snap back if the user moves on.
	//
	// Critical: scheduled after a microtask so a state-rebuild from
	// the path-change effect (which calls `setState` wholesale)
	// finishes first; dispatching into a `setState`-mid-flight view
	// no-ops silently. Microtask order: path effect runs → setState
	// clears old doc → this effect runs → dispatch lands.
	$effect(() => {
		const v = view;
		const folder = workspace.activeFolderPath;
		if (!v || folder === null) {
			return;
		}
		const key = `${folder}::${file.path}`;
		const pending = workspace.pendingJumps.get(key);
		if (!pending) {
			return;
		}
		queueMicrotask(() => {
			const offset = offsetForLspPosition(v, pending);
			v.dispatch({
				selection: EditorSelection.cursor(offset),
				effects: EditorView.scrollIntoView(offset, { y: 'center' }),
			});
			workspace.consumePendingJump(folder, file.path);
		});
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

	// Command palette: focus editor and run local autocomplete (same path as Ctrl+T).
	$effect(() => {
		const t = workspace.autocompleteEditorTick;
		if (workspace.focusedSide !== side) {
			return;
		}
		const v = view;
		if (!v || t === 0 || t === lastHandledAutocompleteEditorTick) {
			return;
		}
		lastHandledAutocompleteEditorTick = t;
		queueMicrotask(() => {
			v.focus();
			void applyAutocompleteFromEditorView(v);
		});
	});

	function baseExtensions() {
		const ec = workspace.editorConfigFor(file.path);
		return [
			lineNumbers(),
			// Code folding. The gutter sits immediately right of the
			// line numbers so the markers stay visually anchored to
			// the line they fold; pulls in `codeFolding()` as a
			// dependency, so we don't add it separately. Languages
			// that ship a Lezer grammar (TS/JS, JSON, Rust, Go,
			// Python, HTML/Svelte, Vue, CSS, Markdown) declare fold
			// ranges via `languageData.foldNodeProp` and get folding
			// for free; legacy `StreamLanguage` modes (TOML, YAML,
			// shell, dockerfile, properties, ignore, dotenv, JSONL)
			// have no fold info and render an empty marker column —
			// same behaviour as VS Code for those grammars.
			//
			// We deliberately do **not** install CM's `foldKeymap`.
			// Its `Ctrl-Alt-[` / `Ctrl-Alt-]` (foldAll / unfoldAll)
			// shadow the AltGr-`[` / AltGr-`]` glyphs on French
			// AZERTY and other AltGr layouts: Linux browsers report
			// AltGr as `ctrlKey + altKey`, and CM's `runHandlers`
			// matches `event.key === '['` against `Ctrl-Alt-[`
			// before any layout heuristic kicks in (the
			// `browser.windows` guard inside CM only suppresses
			// the keyCode-fallback path, not the keyName match),
			// so typing a `[` literal would silently fold the
			// whole file. The `Ctrl-Shift-[` / `]` pair is
			// unreachable on AZERTY anyway. Click the gutter to
			// fold; if a keyboard binding is wanted later, wire
			// one whose key is layout-stable (an F-key, or a
			// `Ctrl+K` leader sequence à la VS Code).
			foldGutter(),
			highlightActiveLine(),
			highlightActiveLineGutter(),
			bracketMatching(),
			closeBrackets(),
			indentOnInput(),
			history(),
			highlightSelectionMatches(),
			// Deleted-file tabs (working-tree copy gone, `file.text`
			// holds the HEAD blob captured at open time) land in the
			// regular Editor when the user clicked their row in the
			// SCM changes tree — see `FileTree.activateRowFromTree`
			// and `EditorPane.showDiff`. The user wants a readable
			// "what was in this file?" view, not a side-by-side
			// against an empty pane, but a stray keystroke followed
			// by Ctrl+S would silently un-delete the file with the
			// HEAD bytes (saveActive doesn't gate on `isDeleted`).
			// Locking the view read-only is the cheapest way to
			// keep the gesture safe — the explicit "View Diff"
			// affordance is still one Ctrl+Shift+D away.
			...(file.isDeleted ? [EditorState.readOnly.of(true), EditorView.editable.of(false)] : []),
			// LSP diagnostics: the gutter extension paints severity
			// markers; `setDiagnostics` gets dispatched by the
			// reactive `$effect` below.
			...lspDiagnosticsExtension(),
			// LSP overview ruler: thin lane on the right edge of
			// the viewport that plots every diagnostic at its
			// scaled vertical position, so errors further down a
			// long file are discoverable without scrolling. Same
			// shape as the git-changes overview, just in a
			// neighbouring CSS lane.
			lspOverviewExtension,
			lspHoverExtension(),
			lspGotoDefinitionExtension({
				jumpTo: (path, position, folder) => workspace.jumpTo(path, position, side, folder),
				resolveExternalUri: (uri) => workspace.resolveExternalUri(uri),
				recordSourcePosition: (path, position) => {
					const folder = workspace.activeFolderPath;
					if (folder !== null) {
						workspace.pushClickNavigation(folder, path, position);
					}
				},
				flash: (msg) => workspace.flash(msg),
			}),
			// F2 LSP rename. The extension owns its own keymap +
			// docked panel; the editor just plugs it in.
			lspRenameExtension(),
			lspPathCompartment.of(filePathFacet.of(file.path)),
			// Git blame annotation for the caret's current line.
			// The compartment reconfigures to feed new blame data
			// as `workspace.blameByPath` updates.
			blameExtension(),
			blameCompartment.of(blameFacet.of(workspace.blameByPath.get(file.path) ?? null)),
			// Per-line git-change indicator: paints the line-number
			// gutter cell background green / blue (added / modified)
			// and adds a thin red top/bottom border on the adjacent
			// line for pure deletions. Replaces the older dedicated
			// wedge gutter — same data source (`workspace.headByPath`
			// via `headTextFacet`), just rendered against the
			// existing line-number column so the chrome stays
			// narrower. To open diff mode: tab toggle, Ctrl+Shift+D,
			// or the SCM panel's diff column.
			gitChangesExtension(),
			headCompartment.of(headTextFacet.of(workspace.headByPath.get(file.path) ?? null)),
			// Ctrl+Space → LSP only. Local autocomplete (Ctrl+T / palette) patches
			// the buffer directly — it is not a CodeMirror completion source.
			//
			// `defaultKeymap: false` is load-bearing: with the default
			// on, `autocompletion()` installs the upstream
			// `completionKeymap` at `Prec.highest` internally (see
			// `completionKeymapExt` in @codemirror/autocomplete), which
			// would shadow our `Prec.high` `logCtrlSpace` tap — the user
			// would press Ctrl+Space, completion would fire, but our
			// "Ctrl+Space pressed" breadcrumb would never log. We spread
			// `completionKeymap` ourselves below; the tap runs first
			// and falls through, so the canonical completion handlers
			// still own popup navigation / accept / escape.
			autocompletion({
				activateOnTyping: false,
				override: [lspCompletionSource],
				defaultKeymap: false,
			}),
			Prec.high(
				keymap.of([
					{ key: 'Ctrl-t', run: runAutocompleteFromShortcut },
					{ mac: 'Ctrl-t', run: runAutocompleteFromShortcut },
					// Tap on Ctrl+Space so the user can see it land
					// in CodeMirror via the diag-logs panel.
					// `logCtrlSpace` returns `false` so the keystroke
					// continues to the `completionKeymap` block below
					// (same `Prec.high`, registered after us, so
					// within-precedence ordering hands Ctrl-Space to
					// it next) and `startCompletion` fires there.
					{ key: 'Ctrl-Space', run: logCtrlSpace },
				]),
			),
			// `completionKeymap` lives at `Prec.high` so its
			// `ArrowDown` / `ArrowUp` / `Enter` / `Escape`
			// handlers beat the corresponding bindings in
			// `defaultKeymap` (which would otherwise move the
			// caret instead of the popup selection). Each handler
			// returns `false` when the popup is closed, so the
			// default-precedence bindings still own those keys for
			// regular editing — they only "win" while the popup is
			// up. Mirrors the upstream `autocompletion()` install
			// (which would put this at `Prec.highest`) but kept at
			// `Prec.high` so any future `Prec.highest` override can
			// still intercept.
			Prec.high(keymap.of([...completionKeymap])),
			keymap.of([
				// Alt+Left / Alt+Right (= file-history back / forward)
				// are handled at the window level in `App.svelte`,
				// not here — they need to fire on diff tabs, image
				// tabs, and anywhere else the user might be focused,
				// not just inside a CodeMirror editor. Keeping the
				// binding out of CM's keymap also avoids the stack-
				// empty-fallback to CM's word-motion default, which
				// was a confusing escape hatch users weren't
				// actually using.
				...closeBracketsKeymap,
				...defaultKeymap,
				...historyKeymap,
				...searchKeymap,
				indentWithTab,
			]),
			themeCompartment.of(moonEditorTheme(workspace.effectiveTheme)),
			languageCompartment.of([]),
			editorConfigCompartment.of(editorConfigExtensions(ec)),
			lineWrapCompartment.of(workspace.lineWrap ? EditorView.lineWrapping : []),
			highlightTabs(),
			EditorView.updateListener.of((update) => {
				if (update.docChanged) {
					const text = update.state.doc.toString();
					if (currentPath !== null) {
						workspace.updateText(currentPath, text);
					}
				}
				if (!update.selectionSet || currentPath === null) {
					return;
				}
				// Publish the active editor's *non-empty* selection
				// to the workspace store so Ctrl+L (and the floating
				// "Add to Coder" hint) can read it without poking
				// the CodeMirror view. Empty selections clear the
				// snapshot — the coder hint shouldn't appear for a
				// caret-only state.
				publishSelection(update);
				const folder = workspace.activeFolderPath;
				if (folder === null) {
					return;
				}
				const pos = lspPositionFromOffset(update.state, update.state.selection.main.head);
				// Classify the selection change: a mouse click produces
				// a transaction annotated `select.pointer`, in which case
				// we push a fresh nav entry (the "you were reading line
				// 10, clicked line 50" bookmark). Everything else —
				// arrow keys, find-next, programmatic selection updates
				// from our own pendingJump dispatch — just drags the
				// current tip along so Alt+Right after a back-nav
				// restores the caret where the user last left it.
				const isClick = update.transactions.some((tr) => {
					const evt = tr.annotation(Transaction.userEvent);
					return typeof evt === 'string' && evt.startsWith('select.pointer');
				});
				if (isClick) {
					workspace.pushClickNavigation(folder, currentPath, pos);
				} else {
					workspace.updateNavTip(folder, currentPath, pos);
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

	// Publish or clear the workspace-level `activeSelection` so
	// `Ctrl+L` and the editor pane's "Add to Coder" hint can read
	// the selection without round-tripping through CodeMirror.
	// Empty (caret-only) selections clear the snapshot. We snapshot
	// the *text* at update time on purpose — Cursor's behaviour:
	// the agent sees what was selected when the user attached, not
	// what the file looks like later.
	function publishSelection(update: { state: EditorState }) {
		if (currentPath === null) {
			workspace.setActiveSelection(null);
			return;
		}
		const sel = update.state.selection.main;
		if (sel.empty) {
			workspace.setActiveSelection(null);
			return;
		}
		const fromLine = update.state.doc.lineAt(sel.from);
		const toLine = update.state.doc.lineAt(sel.to);
		// CodeMirror's `selection.main.to` lives just *past* the
		// last selected character. When the user's drag ends at the
		// start of a line they didn't actually intend to include,
		// we snap back to the previous line so `89-101` doesn't
		// accidentally become `89-102` for an off-by-one drag.
		const effectiveToLineNumber =
			sel.to === toLine.from && toLine.number > fromLine.number ? toLine.number - 1 : toLine.number;
		const text = update.state.doc.sliceString(sel.from, sel.to);
		workspace.setActiveSelection({
			path: currentPath,
			startLine: fromLine.number,
			endLine: effectiveToLineNumber,
			text,
		});
	}

	// CM offset → LSP position. Line numbers are 0-indexed in LSP /
	// the protocol; CM's `line(n)` is 1-indexed, so we subtract.
	// Character is UTF-16 codeunits from line start — matches both
	// CM's internal model and LSP's encoding, so no conversion.
	function lspPositionFromOffset(state: EditorState, offset: number): LspPosition {
		const line = state.doc.lineAt(offset);
		return { line: line.number - 1, character: offset - line.from };
	}

	// Replace the editor's doc with `next` while preserving the
	// caret, scroll, and any other selection-mapped state. The
	// previous implementation dispatched a single
	// `{ from: 0, to: doc.length, insert: next }` change, which
	// CodeMirror is forced to interpret as "the whole document was
	// deleted and replaced" — every selection inside that range
	// collapses to offset 0, plus every per-transaction view
	// extension (LSP didChange, git-changes line diff, blame,
	// language tokeniser) recomputes against the entire new doc.
	//
	// Format-on-save typically rewrites a handful of bytes; a
	// granular character diff anchors the cursor to surviving
	// content and keeps each extension's incremental work
	// proportional to what actually changed. The `diff` package is
	// already a dependency (used by the git-change gutter).
	// Re-attach a serialized CM history to a fresh state seeded
	// with `text`. We route through `EditorState.fromJSON` rather
	// than reaching into `historyField.spec` (not part of CM's
	// public surface) — the JSON shape `{ doc, selection,
	// history }` is what CM's reader expects. We override `doc`
	// with the workspace's current text in case it changed
	// externally while the tab wasn't visible; the offsets in
	// the cursor / scroll restore branch below run their own
	// clamping, and we collapse the selection to the doc start
	// here so `fromJSON` can't reject an out-of-range range
	// (the live selection restore lands a few lines later
	// anyway and overwrites this).
	function buildStateWithHistory(text: string, historyJson: unknown): EditorState {
		const json = { ...(historyJson as object), doc: text, selection: { ranges: [{ anchor: 0, head: 0 }], main: 0 } };
		try {
			return EditorState.fromJSON(json, { extensions: baseExtensions() }, { history: historyField });
		} catch {
			// `fromJSON` rejects malformed history blobs (e.g. a
			// schema change in `@codemirror/commands`) — silently
			// fall back to a history-less state rather than
			// trapping the user with a broken tab. The lost
			// undo stack is recoverable by retyping; a thrown
			// error here is not.
			return EditorState.create({ doc: text, extensions: baseExtensions() });
		}
	}

	function syncDocText(v: EditorView, next: string): void {
		const current = v.state.doc.toString();
		if (current === next) {
			return;
		}
		const parts = diffChars(current, next);
		const changes: { from: number; to: number; insert: string }[] = [];
		let offset = 0;
		for (const part of parts) {
			if (part.added) {
				changes.push({ from: offset, to: offset, insert: part.value });
				continue;
			}
			if (part.removed) {
				changes.push({ from: offset, to: offset + part.value.length, insert: '' });
				offset += part.value.length;
				continue;
			}
			offset += part.value.length;
		}
		if (changes.length === 0) {
			return;
		}
		v.dispatch({ changes });
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
