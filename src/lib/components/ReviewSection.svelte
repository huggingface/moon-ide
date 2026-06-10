<script lang="ts">
	import { onMount } from 'svelte';
	import { Compartment, EditorState, type Extension } from '@codemirror/state';
	import { EditorView, highlightActiveLine, highlightActiveLineGutter, keymap, lineNumbers } from '@codemirror/view';
	import { bracketMatching, foldGutter, indentOnInput, indentUnit } from '@codemirror/language';
	import { defaultKeymap, history, historyKeymap, indentWithTab } from '@codemirror/commands';
	import { closeBrackets, closeBracketsKeymap } from '@codemirror/autocomplete';
	import { highlightSelectionMatches, searchKeymap } from '@codemirror/search';
	import { searchAsYouType } from '../editor/searchAsYouType';
	import { MergeView, diff as rawDiff } from '@codemirror/merge';
	import { ipc } from '../ipc';
	import { workspace, type SplitSide } from '../state.svelte';
	import { highlightTabs } from '../editor/highlightTabs';
	import { languageFor } from '../editor/language';
	import { moonEditorTheme } from '../editor/theme';
	import { diffPureChangeExtension } from '../editor/diffPureChange';
	import { diffGutterTintExtension } from '../editor/diffGutterTint';
	import { diffCollapseContextExtension } from '../editor/diffCollapseContext';
	import { filePathFacet } from '../editor/lsp';
	import { hasGotoModifier, lspGotoDefinitionExtension } from '../editor/lspGotoDefinition';
	import { lspLanguageFor } from '../editor/lspLanguage';
	import {
		commentsForSide,
		reanchorComments,
		reviewCallbacksFacet,
		reviewCommentsExtension,
		reviewCommentsFacet,
		reviewComposerFacet,
		type ReviewCommentCallbacks,
	} from '../editor/reviewComments';
	import type { EditorConfig, GitFileStatus, ReviewComment, ReviewSide } from '../protocol';

	type Props = {
		path: string;
		status: GitFileStatus;
		// Merge-base SHA when reviewing against the default branch;
		// `null` when the active baseline is `head`, in which case
		// `loadBase` reads `git show HEAD:<path>` via
		// `gitHeadContent` instead. The section is keyed off this
		// value in the parent so a baseline flip remounts and
		// rebuilds against the right "before" content.
		mergeBase: string | null;
		// Eager: build the MergeView immediately at mount. Lazy: wait
		// for the first IntersectionObserver hit. The parent passes
		// `eager` for the first 1-2 sections (so the user sees content
		// without scrolling); everything else trickles in as the
		// reader scrolls down. Mounting all N MergeViews up-front on
		// a 100-file PR is the original "this is slow" complaint.
		eager: boolean;
		// Wired by `ReviewView` so its scroll handler can find sections
		// by path. Bound, not a callback, because Svelte 5's `bindable`
		// would be over-engineered for a single ref reach-through.
		registerSection: (path: string, el: HTMLElement | null) => void;
		// Pane the review tab lives in. Cross-file goto-def jumps
		// pass this through to `workspace.jumpTo` so the target
		// opens in the same pane that hosted the review tab — even
		// when focus has moved elsewhere.
		side: SplitSide;
	};

	let { path, status, mergeBase, eager, registerSection, side }: Props = $props();

	let sectionEl: HTMLElement | undefined = $state();
	let host: HTMLDivElement | undefined = $state();
	let merge: MergeView | undefined = $state();
	// Explicit user collapse state. `null` = "follow the reviewed
	// default" (collapsed once marked Viewed); a boolean is an
	// explicit user override that survives until they toggle again.
	// Re-marking Viewed (or a drift that clears it) resets the
	// override so the default takes over again.
	let collapseOverride = $state<boolean | null>(null);
	let mounted = $state(false);
	let loading = $state(false);
	let buildToken = 0;
	// One-shot promise that resolves once the underlying file has
	// been attached as an `OpenFile`. Lazily created on the first
	// keystroke (or first modifier-held hover, for goto-def) so an
	// unscrolled section doesn't pay the load cost. Shared across
	// every caller so racing `readFile`s can't reorder a stale
	// `updateText` over a fresh one.
	let backingReady: Promise<boolean> | null = null;
	// Cleanup for the per-section horizontal scroll mirror set
	// up at the tail of `build`. Stored at component scope so the
	// `onMount` teardown (and the rebuild branch, if ever) can
	// drop the DOM listeners without leaking across remounts.
	let detachHScrollSync: (() => void) | null = null;
	// Cleanup for the one-shot modifier-held mousemove listener
	// that attaches the backing buffer (and thus issues LSP
	// `didOpen`) on first goto-def probe. Self-removes after
	// firing once; this handle covers the unmount-before-fire
	// case so the listener doesn't outlive the section.
	let detachGotoAttach: (() => void) | null = null;

	// Theme + language compartments mirror DiffView's pattern so a
	// theme toggle or language hot-swap reconfigures the live merge
	// view instead of forcing a full rebuild. `ecB` is the right-
	// side editorconfig compartment — only side B is editable, so
	// indent / tab settings only matter for it.
	const langA = new Compartment();
	const langB = new Compartment();
	const themeA = new Compartment();
	const themeB = new Compartment();
	const wrapA = new Compartment();
	const wrapB = new Compartment();
	const ecB = new Compartment();
	// Review-comment facets, one pair of compartments per side so the
	// comment list / open-composer request reconfigure the live
	// MergeView without a rebuild (same pattern as theme / wrap).
	const commentsA = new Compartment();
	const commentsB = new Compartment();
	const composerA = new Compartment();
	const composerB = new Compartment();

	// Per-side open-composer requests. `null` = no composer open.
	// Set from the section keybinding using the current selection;
	// cleared on submit / cancel.
	let composerBase = $state<{ startLine: number; endLine: number } | null>(null);
	let composerWorking = $state<{ startLine: number; endLine: number } | null>(null);

	// This section's comments, split by side. Reactive off the
	// workspace store so a create / edit / delete repaints the cards.
	const baseComments = $derived(commentsForSide(workspace.reviewCommentsForPath(path), 'base'));
	const workingComments = $derived(commentsForSide(workspace.reviewCommentsForPath(path), 'working'));

	// Is this file marked reviewed in the active folder? Drives the
	// header "Viewed" checkbox and the default-collapsed state.
	const reviewed = $derived(workspace.isFileReviewed(path));

	// Effective collapse: an explicit user override wins; otherwise
	// a reviewed file collapses by default (the user signed off).
	const collapsed = $derived(collapseOverride ?? reviewed);

	// Deleted-on-disk rows have no working tree to edit; everything
	// else (modified / added / untracked) is fair game for fixes
	// straight from the review surface. `added` / `untracked` files
	// have an empty left-pane base, so an edit on the right just
	// reduces the diff against an empty before — which is the
	// expected behaviour for "tidy up something I just wrote".
	const editable = $derived(status !== 'deleted');

	// Backing-buffer dirty flag for the section header pip. Sourced
	// from `workspace.openFiles` so it tracks the same in-memory
	// state Ctrl+S would write — saving from inside the section
	// flips this back to clean on the next reactive tick.
	const backingDirty = $derived.by(() => {
		const open = workspace.openFiles.find((f) => f.path === path);
		return open !== undefined && open.kind === 'text' && open.isDirty;
	});

	onMount(() => {
		registerSection(path, sectionEl ?? null);
		if (eager) {
			void build();
		} else {
			// One-shot IO: mount on first visibility. We never
			// destroy the MergeView once mounted — losing the
			// scroll/fold state on scroll-away would be confusing,
			// and the memory cost of an already-built CM6 editor
			// off-screen is small compared to the diff bytes it
			// already paid to compute.
			if (sectionEl) {
				const io = new IntersectionObserver(
					(entries) => {
						for (const entry of entries) {
							if (entry.isIntersecting) {
								io.disconnect();
								void build();
								return;
							}
						}
					},
					// Pre-build ~half a viewport early so the section is
					// painted by the time the user scrolls to it.
					{ rootMargin: '50% 0px' },
				);
				io.observe(sectionEl);
				return () => {
					io.disconnect();
					registerSection(path, null);
					buildToken++;
					detachHScrollSync?.();
					detachHScrollSync = null;
					detachGotoAttach?.();
					detachGotoAttach = null;
					merge?.destroy();
					merge = undefined;
					clearOurSelection();
					clearOurFocus();
				};
			}
		}
		return () => {
			registerSection(path, null);
			buildToken++;
			detachHScrollSync?.();
			detachHScrollSync = null;
			detachGotoAttach?.();
			detachGotoAttach = null;
			merge?.destroy();
			merge = undefined;
			clearOurSelection();
			clearOurFocus();
		};
	});

	// Drop the workspace selection snapshot only when it belongs to
	// *our* section's path. Symmetric with the empty-selection
	// branch in `publishReviewSelection` — keeps a sibling section's
	// selection alive when this one unmounts (e.g. lazy mount churn).
	function clearOurSelection() {
		const current = workspace.activeSelection;
		if (current !== null && current.path === path) {
			workspace.setActiveSelection(null);
		}
	}

	// Same pattern for the focus pointer: a section being torn down
	// while focused (e.g. the user committed the file out of the
	// changes list) shouldn't leave a stale path that the next
	// Ctrl+S would route to a no-longer-editable buffer.
	function clearOurFocus() {
		if (workspace.reviewFocusPath === path) {
			workspace.reviewFocusPath = null;
		}
	}

	// Theme follows the workspace toggle without rebuilding state.
	$effect(() => {
		const theme = workspace.effectiveTheme;
		if (!merge) {
			return;
		}
		merge.a.dispatch({ effects: themeA.reconfigure(moonEditorTheme(theme)) });
		merge.b.dispatch({ effects: themeB.reconfigure(moonEditorTheme(theme)) });
	});

	// Soft-wrap toggle mirrors the regular editor / DiffView. Both
	// sides flip together so the merge alignment stays sane.
	$effect(() => {
		const wrap = workspace.lineWrap;
		if (!merge) {
			return;
		}
		const ext = wrap ? EditorView.lineWrapping : [];
		merge.a.dispatch({ effects: wrapA.reconfigure(ext) });
		merge.b.dispatch({ effects: wrapB.reconfigure(ext) });
	});

	// Live editorconfig reconfigure on side B. The compartment is
	// only populated when the section is editable (see `build`),
	// so the dispatch is a no-op for `deleted` rows whose right
	// pane stays read-only.
	$effect(() => {
		const ec = workspace.editorConfigFor(path);
		if (!merge || !editable) {
			return;
		}
		merge.b.dispatch({ effects: ecB.reconfigure(editorConfigExtensions(ec)) });
	});

	function editorConfigExtensions(ec: EditorConfig): Extension {
		const unit = ec.indent_style === 'tab' ? '\t' : ' '.repeat(Math.max(1, ec.indent_size));
		return [EditorState.tabSize.of(Math.max(1, ec.tab_width)), indentUnit.of(unit)];
	}

	// The merge-base / HEAD SHA this section is reading the "before"
	// side against — recorded on each comment so the publish path
	// knows how far the world has moved. `null` baseline (vs HEAD)
	// records the literal string `'HEAD'`.
	function baselineRev(): string {
		return mergeBase ?? 'HEAD';
	}

	function callbacksForSide(s: ReviewSide): ReviewCommentCallbacks {
		return {
			onSubmit: ({ startLine, endLine, lineText, body }) => {
				workspace.addReviewComment({
					path,
					side: s,
					startLine,
					endLine,
					lineText,
					baselineRev: baselineRev(),
					body,
				});
			},
			onEdit: (id, body) => workspace.editReviewComment(id, body),
			onDelete: (id) => workspace.deleteReviewComment(id),
			onCloseComposer: () => {
				if (s === 'base') {
					composerBase = null;
				} else {
					composerWorking = null;
				}
			},
			onAddAtLine: (line) => {
				const req = { startLine: line, endLine: line };
				if (s === 'base') {
					composerBase = req;
				} else {
					composerWorking = req;
				}
			},
		};
	}

	// Live-reconfigure the comment-list compartments when the store
	// changes. `merge.a` is the base side, `merge.b` the working side.
	$effect(() => {
		const list: readonly ReviewComment[] = baseComments;
		if (merge) {
			merge.a.dispatch({ effects: commentsA.reconfigure(reviewCommentsFacet.of(list)) });
		}
	});
	$effect(() => {
		const list: readonly ReviewComment[] = workingComments;
		if (merge) {
			merge.b.dispatch({ effects: commentsB.reconfigure(reviewCommentsFacet.of(list)) });
		}
	});
	$effect(() => {
		const req = composerBase;
		if (merge) {
			merge.a.dispatch({ effects: composerA.reconfigure(reviewComposerFacet.of(req)) });
		}
	});
	$effect(() => {
		const req = composerWorking;
		if (merge) {
			merge.b.dispatch({ effects: composerB.reconfigure(reviewComposerFacet.of(req)) });
		}
	});

	// Open the composer on whichever side currently has a selection,
	// anchored to the selected line range. Wired to a keybinding on
	// both sides (`Mod-Alt-c`). The base side (left) and working side
	// (right) keep independent composers.
	function openComposer(s: ReviewSide, view: EditorView): boolean {
		const sel = view.state.selection.main;
		const fromLine = view.state.doc.lineAt(sel.from).number;
		const toLineRaw = view.state.doc.lineAt(sel.to).number;
		// Same end-of-line snap as `publishReviewSelection`: a drag
		// ending at the very start of a line didn't mean to include it.
		const toLine = sel.to === view.state.doc.lineAt(sel.to).from && toLineRaw > fromLine ? toLineRaw - 1 : toLineRaw;
		const req = { startLine: fromLine, endLine: toLine };
		if (s === 'base') {
			composerBase = req;
		} else {
			composerWorking = req;
		}
		return true;
	}

	// After a (re)build or a doc change settles, persist any comment
	// whose fingerprint re-anchored to a new line so the stored hint
	// stays fresh next launch. Rendering doesn't depend on this (the
	// fingerprint re-resolves every build), so it runs out of band.
	function persistReanchors() {
		if (!merge) {
			return;
		}
		for (const moved of reanchorComments(merge.a.state, baseComments)) {
			workspace.repinReviewComment(moved.id, moved.startLine, moved.endLine);
		}
		for (const moved of reanchorComments(merge.b.state, workingComments)) {
			workspace.repinReviewComment(moved.id, moved.startLine, moved.endLine);
		}
	}

	async function build() {
		if (mounted || loading || !host) {
			return;
		}
		loading = true;
		const token = ++buildToken;
		const [base, working] = await Promise.all([loadBase(), loadWorking()]);
		if (token !== buildToken || !host) {
			return;
		}
		const firstLine = working.length > 0 ? firstLineOf(working) : firstLineOf(base);
		const lang = await languageFor(path, firstLine);
		if (token !== buildToken || !host) {
			return;
		}

		// Side A (base / HEAD blob) is always read-only — editing the
		// "before" view would be meaningless. Side B (working tree)
		// is editable for everything except `deleted` rows.
		const readOnlyA: Extension[] = [EditorState.readOnly.of(true), EditorView.editable.of(false)];
		const readOnlyB: Extension[] = editable ? [] : [EditorState.readOnly.of(true), EditorView.editable.of(false)];

		const sideA: Extension[] = [
			lineNumbers(),
			diffGutterTintExtension('a'),
			diffCollapseContextExtension,
			foldGutter(),
			diffPureChangeExtension,
			highlightSelectionMatches(),
			searchAsYouType(),
			highlightTabs(),
			bracketMatching(),
			themeA.of(moonEditorTheme(workspace.effectiveTheme)),
			langA.of(lang),
			wrapA.of(workspace.lineWrap ? EditorView.lineWrapping : []),
			// Review comments on the base (deleted / old) side.
			reviewCommentsExtension(),
			reviewCallbacksFacet.of(callbacksForSide('base')),
			commentsA.of(reviewCommentsFacet.of(baseComments)),
			composerA.of(reviewComposerFacet.of(composerBase)),
			keymap.of([{ key: 'Mod-Alt-c', run: (view) => openComposer('base', view) }]),
			...readOnlyA,
		];

		// Working-tree side picks up the regular editor's editing
		// stack — history, bracket close, indent-on-input, the
		// standard keymaps — so typing in a review section feels
		// the same as typing in `Editor.svelte`. Hover / diagnostics
		// / completion are still out of scope (they'd require eager
		// `didOpen` on every visible section, which explodes broker
		// traffic on a 50-file branch); goto-definition is the only
		// LSP affordance wired here and it only attaches the buffer
		// on the first modifier-held hover (see `maybeAttachForGoto`
		// below) — most sections will never trigger it.
		const ec = workspace.editorConfigFor(path);
		const editingExtensions: Extension[] = editable
			? [
					highlightActiveLineGutter(),
					closeBrackets(),
					indentOnInput(),
					history(),
					ecB.of(editorConfigExtensions(ec)),
					keymap.of([
						...closeBracketsKeymap,
						...defaultKeymap,
						...historyKeymap,
						...searchKeymap,
						indentWithTab,
						// Ctrl+S inside a review section saves the
						// underlying file, not the synthetic
						// `review://` buffer that `saveActive` would
						// hit via the global Ctrl+S handler. The
						// binding lives on the section's CM editor
						// so it only fires when focus is here;
						// elsewhere the global handler still owns
						// the keystroke.
						{
							key: 'Mod-s',
							run: () => {
								void workspace.saveReviewSection(path);
								return true;
							},
						},
					]),
				]
			: [];

		// Focus tracker: publishes the section's path to the
		// workspace on focus and clears it on blur. `saveActive`
		// reads this when the active tab is the synthetic review
		// buffer so global Ctrl+S lands on the file the user is
		// editing, not on the empty `review://` placeholder.
		// Symmetric clear-only-if-ours rule mirrors
		// `clearOurSelection`: a quick focus hop from this
		// section to a sibling fires the sibling's `focus`
		// before our `blur`, so an unconditional clear in our
		// blur handler would stomp the sibling's claim.
		const focusListener = EditorView.domEventHandlers({
			focus: () => {
				workspace.reviewFocusPath = path;
				return false;
			},
			blur: () => {
				if (workspace.reviewFocusPath === path) {
					workspace.reviewFocusPath = null;
				}
				return false;
			},
		});

		// LSP goto-definition. The extension itself probes the
		// server on modifier-held mousemove and on modifier-held
		// click; both code paths call `ipc.lsp.definition(path, …)`
		// which returns `null` until the broker has seen a
		// `didOpen` for `path`. We wire `ensureBackingBuffer`
		// behind a one-shot `mousemove` listener (further down in
		// `attachGotoListeners`) so the `didOpen` only fires once
		// the user is actually probing this section with the
		// modifier held — the cost stays proportional to user
		// intent, not section count.
		//
		// The jump itself routes through `workspace.jumpTo`, which
		// queues a pending caret position and calls `openFile` on
		// the target. Since the target is never the synthetic
		// `review://` path (only a real file path can come back
		// from `textDocument/definition`), the target opens as a
		// regular editor tab in the active pane — replacing the
		// review tab on the same side. Exactly the "leave review
		// mode and land on the function" behaviour we want.
		const gotoExt: Extension[] =
			lspLanguageFor(path) === null
				? []
				: [
						filePathFacet.of(path),
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
					];

		const sideB: Extension[] = [
			lineNumbers(),
			diffGutterTintExtension('b'),
			diffCollapseContextExtension,
			foldGutter(),
			diffPureChangeExtension,
			highlightActiveLine(),
			highlightSelectionMatches(),
			searchAsYouType(),
			highlightTabs(),
			bracketMatching(),
			themeB.of(moonEditorTheme(workspace.effectiveTheme)),
			langB.of(lang),
			wrapB.of(workspace.lineWrap ? EditorView.lineWrapping : []),
			focusListener,
			// Review comments on the working (added / context) side —
			// the common case. The keybinding works regardless of
			// editability so a `deleted` row's right pane (empty) still
			// no-ops cleanly rather than swallowing the chord.
			reviewCommentsExtension(),
			reviewCallbacksFacet.of(callbacksForSide('working')),
			commentsB.of(reviewCommentsFacet.of(workingComments)),
			composerB.of(reviewComposerFacet.of(composerWorking)),
			keymap.of([{ key: 'Mod-Alt-c', run: (view) => openComposer('working', view) }]),
			...gotoExt,
			...editingExtensions,
			// Selection-publish + edit-publish hook on the working-
			// tree side. Selection routes to `Ctrl+L` (same as the
			// regular editor / DiffView's right pane); doc changes
			// route through `workspace.updateText`, lazily attaching
			// a backing `OpenFile` on first keystroke so the dirty
			// flag, fingerprint, and `Ctrl+S` machinery all have a
			// target. We only wire the right side: the left is the
			// base/HEAD snapshot and stays read-only.
			EditorView.updateListener.of((update) => {
				if (update.selectionSet) {
					publishReviewSelection(update.state);
				}
				if (update.docChanged && editable) {
					const next = update.state.doc.toString();
					// Lazily attach the underlying file as an
					// `OpenFile` on the first keystroke so
					// `workspace.updateText` (and the Ctrl+S save
					// path) have a target. We chain every
					// keystroke off the same `backingReady`
					// promise so two rapid edits before the load
					// resolves can't reorder a stale `updateText`
					// over a fresh one (the chain serialises them).
					if (backingReady === null) {
						backingReady = workspace.ensureBackingBuffer(path);
					}
					backingReady = backingReady.then((ok) => {
						if (ok) {
							workspace.updateText(path, next);
						}
						return ok;
					});
					// Re-anchor working-side comments against the edited
					// doc so their stored line hints follow the text.
					persistReanchors();
				}
			}),
			...readOnlyB,
		];

		detachHScrollSync?.();
		detachHScrollSync = null;

		merge = new MergeView({
			a: { doc: base, extensions: sideA },
			b: { doc: working, extensions: sideB },
			parent: host,
			// Built-in change-bar gutter replaced by line-number
			// cell tinting via `diffGutterTintExtension` — see
			// `DiffView.svelte` for the same call site and
			// rationale.
			gutter: false,
			highlightChanges: true,
			// No revert controls — the review tab is read-only;
			// the user can't apply A→B or vice versa from here.
			//
			// Aggregated review view collapses long unchanged
			// regions behind `… N unchanged lines` placeholders.
			// In `DiffView` (single-file, one-tab-at-a-time) we
			// expanded everything because the change-bar gutter +
			// overview ruler already tell you "where the diffs
			// are" and the placeholders get in the way of Ctrl+F /
			// scroll. Here it's the opposite trade-off: on a
			// 30-file PR with a 2000-line file that only changed
			// 20 lines, an uncollapsed mount stalls the scroll
			// and forces the user to skim past acres of unchanged
			// code to reach the next file. `margin: 3` keeps
			// hunks readable (same as `git diff -U3`); `minSize:
			// 5` only folds runs of ≥5 unchanged lines so tiny
			// gaps between adjacent hunks stay expanded.
			collapseUnchanged: { margin: 3, minSize: 5 },
			diffConfig: {
				// See DiffView.svelte for the rationale: raw diff
				// keeps highlights aligned to single change spans
				// rather than fusing across short matched substrings.
				override: rawDiff,
			},
		});

		detachHScrollSync = wireHorizontalScrollSync(merge.a.scrollDOM, merge.b.scrollDOM);

		// One-shot listener that attaches the backing buffer (and
		// thus issues LSP `didOpen`) the moment the user holds the
		// goto-def modifier over this section's right pane. Keeps
		// the broker silent for sections the user only scrolls
		// past — `ensureBackingBuffer` is itself idempotent, so a
		// duplicate fire (e.g. via the editing path firing first)
		// is harmless. Mouse-hover, not just key-down, so a user
		// who Ctrl+Tabs out of the window and back doesn't trigger
		// every visible section's load at once.
		if (lspLanguageFor(path) !== null && status !== 'deleted') {
			const onMove = (event: MouseEvent) => {
				if (!hasGotoModifier(event)) {
					return;
				}
				detachGotoAttach?.();
				detachGotoAttach = null;
				if (backingReady === null) {
					backingReady = workspace.ensureBackingBuffer(path);
				}
			};
			merge.b.scrollDOM.addEventListener('mousemove', onMove);
			detachGotoAttach = () => {
				merge?.b.scrollDOM.removeEventListener('mousemove', onMove);
			};
		}

		mounted = true;
		loading = false;
		// Settle any drifted comment hints once the diff is built.
		persistReanchors();
	}

	/**
	 * Bidirectional `scrollLeft` mirror between the two `.cm-scroller`
	 * elements of the section's MergeView. Same pattern as
	 * `DiffView.svelte`'s `wireHorizontalScrollSync`: when long
	 * lines force one side into horizontal overflow, dragging
	 * either side's bar (or wheel-scrolling horizontally) drags
	 * the other in lockstep so the aligned chunks line up
	 * visually.
	 *
	 * Echoes from our own programmatic writes are identified by
	 * value (not by a time-windowed flag) so unequal `scrollWidth`s
	 * — the wider side scrolling past the narrower's max — don't
	 * race the rAF-release and snap the wider side back to the
	 * clamped value. See `DiffView.svelte` for the long-form
	 * rationale.
	 *
	 * Returns a cleanup that the section's onMount teardown
	 * invokes on unmount.
	 */
	function wireHorizontalScrollSync(a: HTMLElement, b: HTMLElement): () => void {
		const expected = new WeakMap<HTMLElement, number>();
		const handle = (from: HTMLElement, to: HTMLElement) => {
			const pending = expected.get(from);
			if (pending !== undefined && from.scrollLeft === pending) {
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

	async function loadBase(): Promise<string> {
		// `added` rows have no merge-base blob; an untracked file by
		// definition isn't in git yet either. Skip the fetch — left
		// side renders empty so the diff reads as a pure addition.
		if (status === 'added' || status === 'untracked') {
			return '';
		}
		try {
			// `mergeBase === null` is the "vs HEAD" baseline (the
			// review tab opened against the working tree without
			// flipping to default-branch mode). Read the file at
			// HEAD instead of at the merge-base SHA.
			const content =
				mergeBase !== null ? await ipc.fs.gitRefContent(mergeBase, path) : await ipc.fs.gitHeadContent(path);
			return content ?? '';
		} catch {
			return '';
		}
	}

	async function loadWorking(): Promise<string> {
		// Deleted in the working tree → no bytes to show on the right.
		if (status === 'deleted') {
			return '';
		}
		// Prefer the open buffer when one exists. Reflects unsaved
		// edits in the review (matches the working-tree intent: "what
		// would I be committing right now?"). Falls back to a fresh
		// read otherwise.
		const open = workspace.openFiles.find((f) => f.path === path);
		if (open && open.kind === 'text' && !open.isDeleted) {
			return open.text;
		}
		try {
			const result = await ipc.fs.readFile(path);
			if (result.text === null) {
				return '';
			}
			return result.text;
		} catch {
			return '';
		}
	}

	function firstLineOf(text: string): string {
		const idx = text.indexOf('\n');
		return idx === -1 ? text : text.slice(0, idx);
	}

	function fileName(p: string): string {
		const slash = p.lastIndexOf('/');
		return slash === -1 ? p : p.slice(slash + 1);
	}

	function dirName(p: string): string {
		const slash = p.lastIndexOf('/');
		return slash === -1 ? '' : p.slice(0, slash);
	}

	function statusLabel(s: GitFileStatus): string {
		switch (s) {
			case 'added':
				return 'A';
			case 'modified':
				return 'M';
			case 'deleted':
				return 'D';
			case 'untracked':
				return 'U';
			default:
				return '';
		}
	}

	function openInEditor() {
		// Deleted rows can't open as a normal editor tab (there's
		// nothing to edit). Skip the action — the section already
		// shows the HEAD content on the left.
		if (status === 'deleted') {
			return;
		}
		void workspace.openFile(path);
	}

	function toggleCollapsed() {
		collapseOverride = !collapsed;
	}

	function toggleReviewed() {
		const next = !reviewed;
		// Re-marking Viewed clears any manual expand/collapse override
		// so the reviewed-default (collapsed) takes effect; un-marking
		// likewise resets so the section re-expands.
		collapseOverride = null;
		void workspace.setFileReviewed(path, next);
	}

	/**
	 * Mirror of `Editor.svelte`'s `publishSelection` and
	 * `DiffView.svelte`'s `publishDiffSelection` for the review
	 * section's working-tree pane. Empty selections clear the
	 * snapshot so `Ctrl+L` doesn't attach a stale highlight from
	 * a section the user moved away from. Same off-by-one snap
	 * when the drag ends at the start of a line the user didn't
	 * actually mean to include.
	 */
	function publishReviewSelection(state: EditorState) {
		const sel = state.selection.main;
		if (sel.empty) {
			// Only clear if the workspace snapshot is currently ours —
			// otherwise we'd stomp on a selection another section just
			// published. Side-by-side MergeViews don't share focus, but
			// CodeMirror still dispatches `selectionSet` updates when a
			// view loses focus and resets its selection.
			const current = workspace.activeSelection;
			if (current !== null && current.path === path) {
				workspace.setActiveSelection(null);
			}
			return;
		}
		const fromLine = state.doc.lineAt(sel.from);
		const toLine = state.doc.lineAt(sel.to);
		const effectiveToLineNumber =
			sel.to === toLine.from && toLine.number > fromLine.number ? toLine.number - 1 : toLine.number;
		const text = state.doc.sliceString(sel.from, sel.to);
		workspace.setActiveSelection({
			path,
			startLine: fromLine.number,
			endLine: effectiveToLineNumber,
			text,
		});
	}
</script>

<section
	bind:this={sectionEl}
	class="review-section"
	class:collapsed
	data-review-path={path}
	aria-label={`Diff of ${path}`}
>
	<header class="hdr">
		<button
			type="button"
			class="caret"
			aria-expanded={!collapsed}
			aria-label={collapsed ? 'Expand diff' : 'Collapse diff'}
			onclick={toggleCollapsed}
		>
			<span aria-hidden="true">{collapsed ? '▸' : '▾'}</span>
		</button>
		<span class="status status-{status}" title={`Status: ${status}`} aria-label={`Status ${status}`}>
			{statusLabel(status)}
		</span>
		<button type="button" class="path" title={`Open ${path}`} onclick={openInEditor}>
			{#if dirName(path)}<span class="dir">{dirName(path)}/</span>{/if}<span class="name">{fileName(path)}</span>
		</button>
		{#if backingDirty}
			<span class="dirty" title="Unsaved edits — Ctrl+S to save" aria-label="Unsaved edits">●</span>
		{/if}
		<label class="viewed" title="Mark this file reviewed. A new commit that changes it clears the mark.">
			<input type="checkbox" checked={reviewed} onchange={toggleReviewed} />
			<span>Viewed</span>
		</label>
	</header>
	{#if !collapsed}
		<div class="body" bind:this={host}></div>
		{#if !mounted && loading}
			<div class="placeholder">Loading diff…</div>
		{:else if !mounted}
			<div class="placeholder">Scroll to load diff</div>
		{/if}
	{/if}
</section>

<style>
	.review-section {
		display: flex;
		flex-direction: column;
		border: 1px solid var(--m-border);
		border-radius: 6px;
		background: var(--m-bg);
		overflow: hidden;
		/* Keep a scrolled-to section clear of the sticky banner so
		 * its header isn't hidden behind it on `scrollIntoView`. */
		scroll-margin-top: var(--m-review-banner-h, 12px);
	}
	.hdr {
		display: flex;
		align-items: center;
		gap: 8px;
		padding: 6px 10px;
		background: var(--m-bg-1);
		border-bottom: 1px solid var(--m-border);
		position: sticky;
		/* Park just below the review view's sticky banner instead of
		 * sliding under it. `--m-review-banner-h` is defined on the
		 * scrolling `.review-view`; falls back to 0 outside it. */
		top: var(--m-review-banner-h, 0);
		z-index: 2;
	}
	.review-section.collapsed .hdr {
		border-bottom: none;
	}
	.caret {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		width: 18px;
		height: 18px;
		padding: 0;
		background: transparent;
		border: none;
		color: var(--m-fg-muted);
		cursor: pointer;
		font-size: 12px;
		line-height: 1;
	}
	.caret:hover {
		color: var(--m-fg);
	}
	.status {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		min-width: 16px;
		height: 16px;
		padding: 0 4px;
		font-size: 10px;
		font-weight: 600;
		border-radius: 3px;
		font-family: var(--m-font-mono, monospace);
	}
	.status-added {
		color: var(--m-git-added, #4ec9b0);
		background: color-mix(in srgb, var(--m-git-added, #4ec9b0) 18%, transparent);
	}
	.status-modified {
		color: var(--m-git-modified, #e2c08d);
		background: color-mix(in srgb, var(--m-git-modified, #e2c08d) 18%, transparent);
	}
	.status-deleted {
		color: var(--m-git-deleted, #f48771);
		background: color-mix(in srgb, var(--m-git-deleted, #f48771) 18%, transparent);
	}
	.status-untracked {
		color: var(--m-fg-muted);
		background: color-mix(in srgb, var(--m-fg-muted) 18%, transparent);
	}
	.path {
		flex: 1;
		min-width: 0;
		display: inline-flex;
		align-items: baseline;
		gap: 0;
		padding: 2px 4px;
		background: transparent;
		border: none;
		color: var(--m-fg);
		font-family: var(--m-font-mono, monospace);
		font-size: 12px;
		text-align: left;
		cursor: pointer;
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}
	.path:hover .name {
		text-decoration: underline;
	}
	.dir {
		color: var(--m-fg-muted);
	}
	.name {
		color: var(--m-fg);
	}
	.dirty {
		color: var(--m-accent, #4ec9b0);
		font-size: 14px;
		line-height: 1;
		margin-left: 4px;
		flex: 0 0 auto;
	}
	.viewed {
		display: inline-flex;
		align-items: center;
		gap: 5px;
		flex: 0 0 auto;
		margin-left: auto;
		padding: 1px 4px;
		font-size: 11px;
		color: var(--m-fg-muted);
		cursor: pointer;
		user-select: none;
	}
	.viewed:hover {
		color: var(--m-fg);
	}
	.viewed input {
		margin: 0;
		cursor: pointer;
	}
	.body {
		display: flex;
		min-height: 0;
	}
	.placeholder {
		padding: 12px;
		color: var(--m-fg-muted);
		font-size: 12px;
		font-style: italic;
		text-align: center;
	}
	/* `@codemirror/merge` ships `.cm-mergeView` as the scrolling
	 * container (`overflow-y: auto`). Inside the stacked review
	 * the *outer* `.review-view` is the scroller; relax the merge
	 * package's overflow so each section grows to its content
	 * height and the user gets one long page of diffs instead of
	 * a nested scrollbar per file. Two layout side-effects to
	 * keep an eye on:
	 *
	 *   1. The horizontal scrollbar that DiffView re-parents to a
	 *      sticky strip — we don't bother here, the review view
	 *      is read-only and very-wide lines just word-wrap (or
	 *      overflow into a per-section horizontal scroll, which
	 *      is fine for a quick read).
	 *   2. MergeView's chunk-align spacer pass still works as long
	 *      as each side reports its natural content height, which
	 *      `overflow: visible` lets through.
	 */
	.review-section :global(.cm-mergeView) {
		flex: 1;
		min-width: 0;
		overflow: visible;
	}
	.review-section :global(.cm-mergeViewEditors) {
		min-width: 0;
	}
	.review-section :global(.cm-editor) {
		outline: none;
	}
	.review-section :global(.cm-scroller) {
		overflow-x: auto;
	}
	/* Character-level change marker: see the matching rules in
	 * `DiffView.svelte` for the full rationale. Short version:
	 * the library default is a 2px bottom-edge gradient that
	 * reads as a loud underline doubled with LSP lint
	 * squigglies. Swap for a soft same-hue background
	 * (GitHub-style inline diff highlight) using our palette
	 * tokens so theme flips track. `!important` beats the
	 * package's themed rules without a fragile selector-
	 * specificity arms race. */
	.review-section :global(.cm-merge-b .cm-changedText) {
		background: color-mix(in srgb, var(--m-success) 22%, transparent) !important;
		border-radius: 2px;
	}
	.review-section :global(.cm-merge-a .cm-changedText),
	.review-section :global(.cm-deletedChunk .cm-deletedText) {
		background: color-mix(in srgb, var(--m-danger) 22%, transparent) !important;
		border-radius: 2px;
	}
	.review-section :global(.cm-merge-b .cm-deletedText) {
		background: color-mix(in srgb, var(--m-danger) 22%, transparent) !important;
	}
	/* Pure-added / pure-deleted lines: the line-level change is
	 * already conveyed by the gutter bar plus the line tint, so
	 * layering the per-character marker on top doubles up the
	 * same hue across the entire line. `diffPureChange.ts`
	 * adds the `.cm-moon-pure-change` line decoration on any
	 * line whose content is wholly inside `Change` spans —
	 * whole-chunk pure adds / deletes plus all-new / all-removed
	 * lines living inside an otherwise-modified chunk. Lines
	 * with surviving common substrings keep their per-character
	 * markers so the substring-vs-surrounding-text distinction
	 * stays visible. */
	.review-section :global(.cm-moon-pure-change .cm-changedText) {
		background: transparent !important;
	}

	/* Inline review-comment cards + composer. Rendered by the CM
	 * `reviewComments` extension into the editor DOM, so the
	 * selectors are `:global`. Block widgets sit below their
	 * anchored line; keep them visually distinct from code without
	 * shouting. */
	.review-section :global(.cm-review-card),
	.review-section :global(.cm-review-composer) {
		margin: 4px 8px 4px 28px;
		border: 1px solid var(--m-border);
		border-radius: 6px;
		background: var(--m-bg-1);
		font-family: var(--m-font-sans, system-ui, sans-serif);
		font-size: 12px;
		overflow: hidden;
	}
	.review-section :global(.cm-review-card-stale) {
		opacity: 0.7;
		border-style: dashed;
	}
	.review-section :global(.cm-review-card-head) {
		display: flex;
		align-items: center;
		gap: 8px;
		padding: 5px 8px;
		border-bottom: 1px solid var(--m-border);
		background: var(--m-bg-2, var(--m-bg-1));
	}
	.review-section :global(.cm-review-card-author) {
		font-weight: 600;
		color: var(--m-fg);
	}
	.review-section :global(.cm-review-card-time) {
		color: var(--m-fg-muted);
		font-size: 11px;
	}
	.review-section :global(.cm-review-card-staleflag) {
		color: var(--m-git-modified, #e2c08d);
		font-size: 10px;
		text-transform: uppercase;
		letter-spacing: 0.03em;
	}
	.review-section :global(.cm-review-card-spacer) {
		flex: 1;
	}
	.review-section :global(.cm-review-card-btn) {
		padding: 1px 6px;
		background: transparent;
		border: 1px solid transparent;
		border-radius: 4px;
		color: var(--m-fg-muted);
		font-size: 11px;
		cursor: pointer;
	}
	.review-section :global(.cm-review-card-btn:hover) {
		color: var(--m-fg);
		border-color: var(--m-border);
	}
	.review-section :global(.cm-review-card-body) {
		padding: 6px 8px;
		color: var(--m-fg);
		word-break: break-word;
		line-height: 1.4;
	}
	/* Tighten markdown block spacing inside the compact card. */
	.review-section :global(.cm-review-markdown > :first-child) {
		margin-top: 0;
	}
	.review-section :global(.cm-review-markdown > :last-child) {
		margin-bottom: 0;
	}
	.review-section :global(.cm-review-markdown p) {
		margin: 0 0 6px;
	}
	.review-section :global(.cm-review-markdown pre) {
		margin: 6px 0;
		padding: 6px 8px;
		background: var(--m-bg);
		border: 1px solid var(--m-border);
		border-radius: 4px;
		overflow-x: auto;
	}
	.review-section :global(.cm-review-markdown code) {
		font-family: var(--m-font-mono, monospace);
		font-size: 11px;
	}
	/* Hover "+" gutter: the column collapses to nothing on rows
	 * without a marker, so it only occupies width on the active row. */
	.review-section :global(.cm-review-add-gutter) {
		padding: 0;
	}
	.review-section :global(.cm-review-add-btn) {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		width: 16px;
		height: 16px;
		padding: 0;
		margin: 0 1px;
		background: var(--m-accent, #4ec9b0);
		border: none;
		border-radius: 3px;
		color: var(--m-bg);
		font-size: 13px;
		font-weight: 700;
		line-height: 1;
		cursor: pointer;
	}
	.review-section :global(.cm-review-add-btn:hover) {
		filter: brightness(1.1);
	}
	.review-section :global(.cm-review-composer) {
		padding: 6px;
	}
	.review-section :global(.cm-review-composer-input) {
		display: block;
		width: 100%;
		box-sizing: border-box;
		resize: vertical;
		padding: 6px 8px;
		background: var(--m-bg);
		border: 1px solid var(--m-border);
		border-radius: 4px;
		color: var(--m-fg);
		font-family: var(--m-font-sans, system-ui, sans-serif);
		font-size: 12px;
		line-height: 1.4;
	}
	.review-section :global(.cm-review-composer-input:focus) {
		outline: none;
		border-color: var(--m-accent, #4ec9b0);
	}
	.review-section :global(.cm-review-composer-actions) {
		display: flex;
		justify-content: flex-end;
		gap: 6px;
		margin-top: 6px;
	}
	.review-section :global(.cm-review-composer-btn) {
		padding: 3px 10px;
		background: var(--m-bg-1);
		border: 1px solid var(--m-border);
		border-radius: 4px;
		color: var(--m-fg);
		font-size: 11px;
		cursor: pointer;
	}
	.review-section :global(.cm-review-composer-btn:hover) {
		border-color: var(--m-fg-muted);
	}
	.review-section :global(.cm-review-composer-submit) {
		background: var(--m-accent, #4ec9b0);
		border-color: var(--m-accent, #4ec9b0);
		color: var(--m-bg);
		font-weight: 600;
	}
</style>
