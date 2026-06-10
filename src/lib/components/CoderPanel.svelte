<script lang="ts">
	import { onMount, tick } from 'svelte';
	import { SvelteSet } from 'svelte/reactivity';
	import { confirm } from '@tauri-apps/plugin-dialog';
	import { readImage, writeText as clipboardWriteText } from '@tauri-apps/plugin-clipboard-manager';
	import { openUrl } from '@tauri-apps/plugin-opener';
	import { coder, type CoderRow } from '../coder.svelte';
	import { frontendLog } from '../logs.svelte';
	import { slack } from '../slack.svelte';
	import { workspace } from '../state.svelte';
	import CoderConnectModal from './CoderConnectModal.svelte';
	import CoderMarkdown from './CoderMarkdown.svelte';
	import CoderModelSettingsModal from './CoderModelSettingsModal.svelte';
	import HfBucketSettingsModal from './HfBucketSettingsModal.svelte';
	import CoderThinking from './CoderThinking.svelte';
	import ToolBodyAskUser from './ToolBodyAskUser.svelte';
	import ToolBodyEditFile from './ToolBodyEditFile.svelte';
	import ToolBodyGrep from './ToolBodyGrep.svelte';
	import ToolBodyListDir from './ToolBodyListDir.svelte';
	import ToolBodyReadFile from './ToolBodyReadFile.svelte';
	import ToolBodyTodoWrite from './ToolBodyTodoWrite.svelte';
	import ToolBodyWebFetch from './ToolBodyWebFetch.svelte';
	import ToolBodyWebSearch from './ToolBodyWebSearch.svelte';
	import ToolBodyWriteFile from './ToolBodyWriteFile.svelte';
	import TerminalTargetIcon from './TerminalTargetIcon.svelte';
	import ContextRing from './ContextRing.svelte';
	import CoderTodoPill from './CoderTodoPill.svelte';
	import ChatBubbleIcon from './icons/ChatBubbleIcon.svelte';
	import SettingsIcon from './icons/SettingsIcon.svelte';
	import SignOutIcon from './icons/SignOutIcon.svelte';
	import PlusIcon from './icons/PlusIcon.svelte';
	import CloudUploadIcon from './icons/CloudUploadIcon.svelte';
	import CloudSyncIcon from './icons/CloudSyncIcon.svelte';
	import ExternalLinkIcon from './icons/ExternalLinkIcon.svelte';
	import ListIcon from './icons/ListIcon.svelte';
	import FileIcon from './icons/FileIcon.svelte';
	import TrashIcon from './icons/TrashIcon.svelte';
	import CodeIcon from './icons/CodeIcon.svelte';
	import RevertIcon from './icons/RevertIcon.svelte';
	import EditIcon from './icons/EditIcon.svelte';
	import { ipc } from '../ipc';
	import { formatError, type FileSearchResult } from '../protocol';
	import { textInputUndo } from '../actions/textInputUndo';

	let scrollEl: HTMLDivElement | undefined = $state();
	let composer: HTMLTextAreaElement | undefined = $state();

	// Whether the model-picker popover is currently mounted. Local
	// to the header because no other surface opens it; keeping it
	// off the global store also means closing the popover doesn't
	// emit a Svelte re-render of every panel consumer.
	let modelSettingsOpen = $state(false);
	let hubSettingsOpen = $state(false);

	// Full-size preview of a pasted image, opened by clicking its
	// thumbnail in the transcript. We render an in-app lightbox
	// rather than `target="_blank"`, which the Tauri webview can't
	// honour for `data:` URLs.
	let lightboxUrl = $state<string | null>(null);

	// Per-session shell-target override popover (Auto / Force host).
	// Anchored to the bash-target pip in the header.
	let shellTargetOpen = $state(false);
	let shellTargetWrap = $state<HTMLDivElement | null>(null);

	async function selectShellTarget(forceHost: boolean): Promise<void> {
		shellTargetOpen = false;
		if (forceHost === coder.forceHostOverride) {
			return;
		}
		await coder.setBashTargetOverride(forceHost);
	}

	// Outside-click / Escape dismiss for the shell-target popover.
	$effect(() => {
		if (!shellTargetOpen) {
			return;
		}
		const onPointerDown = (event: PointerEvent) => {
			if (shellTargetWrap?.contains(event.target as Node)) {
				return;
			}
			shellTargetOpen = false;
		};
		const onKey = (event: KeyboardEvent) => {
			if (event.key === 'Escape') {
				shellTargetOpen = false;
			}
		};
		window.addEventListener('pointerdown', onPointerDown);
		window.addEventListener('keydown', onKey);
		return () => {
			window.removeEventListener('pointerdown', onPointerDown);
			window.removeEventListener('keydown', onKey);
		};
	});

	// Escape dismiss for the full-size image lightbox.
	$effect(() => {
		if (lightboxUrl === null) {
			return;
		}
		const onKey = (event: KeyboardEvent) => {
			if (event.key === 'Escape') {
				lightboxUrl = null;
			}
		};
		window.addEventListener('keydown', onKey);
		return () => window.removeEventListener('keydown', onKey);
	});

	onMount(() => {
		void coder.refreshStatus();
		void coder.loadHubBinding();
	});

	function hubRowState(sessionId: string): 'idle' | 'syncing' | 'synced' | 'failed' {
		const s = coder.hubSyncState[sessionId];
		if (!s) {
			return coder.hubBucket?.uploaded[sessionId] ? 'synced' : 'idle';
		}
		return s.phase;
	}

	function hubRowTitle(sessionId: string): string {
		const bucket = coder.hubBucket;
		if (!bucket) {
			return 'Upload to Hugging Face';
		}
		const target = `${bucket.namespace}/${bucket.name}`;
		const live = coder.hubSyncState[sessionId];
		if (live?.phase === 'syncing') {
			return `Uploading to ${target}…`;
		}
		if (live?.phase === 'failed') {
			return `Upload failed: ${live.error}`;
		}
		const marker = bucket.uploaded[sessionId];
		if (marker || live?.phase === 'synced') {
			return `Synced to ${target}`;
		}
		return `Upload to ${target}`;
	}

	async function onUploadSession(event: MouseEvent, sessionId: string): Promise<void> {
		event.stopPropagation();
		try {
			await coder.uploadSessionToHub(sessionId);
		} catch (err) {
			workspace.flash(`Hub upload failed: ${err instanceof Error ? err.message : String(err)}`);
		}
	}

	/**
	 * Open the trace's Hub URL in the host's default browser
	 * (plain click) or copy it to the clipboard (Alt-click).
	 * Surfaced per-session only when the session has been
	 * uploaded — i.e. there's an `uploaded[id]` marker on the
	 * bucket binding. The button is hidden in any other state, so
	 * a click should never land here without a valid URL, but we
	 * still surface the typed error as a flash for safety.
	 */
	async function onOpenTraceOnHub(event: MouseEvent, sessionId: string): Promise<void> {
		event.stopPropagation();
		let url: string;
		try {
			url = await coder.hubSessionUrl(sessionId);
		} catch (err) {
			workspace.flash(`Could not resolve Hub URL: ${formatError(err)}`);
			return;
		}
		if (event.altKey) {
			// Prefer the Tauri clipboard plugin: `navigator.clipboard`
			// is unreliable on WebKitGTK (silently no-ops when the
			// triggering element isn't a focused input), and Alt-click
			// fires on a button that doesn't take focus. Fall back to
			// `navigator.clipboard` only if the plugin call throws,
			// and surface a flash on hard failure rather than
			// swallowing — matches the `Copy path` pattern in
			// EditorTabs / FileTree.
			try {
				await clipboardWriteText(url);
				workspace.flash('Trace URL copied to clipboard.');
			} catch {
				try {
					await navigator.clipboard.writeText(url);
					workspace.flash('Trace URL copied to clipboard.');
				} catch {
					workspace.flash('Could not copy trace URL.');
				}
			}
			return;
		}
		void openUrl(url);
	}

	// Keep the store's `composerEl` reference in sync with the
	// live textarea node — the store needs it so Ctrl+L can
	// insert an `@`-token at the caret without prop-drilling a
	// callback. Setting / clearing happens on every mount and
	// unmount of the textarea (it remounts when the user swaps
	// between session view and the sessions list).
	$effect(() => {
		coder.composerEl = composer ?? null;
		return () => {
			if (coder.composerEl === composer) {
				coder.composerEl = null;
			}
		};
	});

	// Re-probe `coder_status` when the active folder switches so the
	// host-vs-container indicator pip updates without waiting for the
	// next user action. Reading `workspace.activeFolder?.host` here
	// makes the effect re-run on either path or host change.
	$effect(() => {
		const _kind = workspace.activeFolder?.host ?? null;
		void _kind;
		void coder.refreshStatus();
	});

	// Post-flush marker for folder-swap profiling: fires after Svelte
	// has reconciled the panel for any of the listed dependencies.
	// Pair the timestamp with `moon:setActiveFolder.start` in a
	// devtools timeline to localize where the panel's reconciliation
	// lands in the cascade — when one of the giant style recalcs
	// fires shortly after this mark, the transcript / sessions render
	// is the trigger.
	$effect(() => {
		void coder.activeFolderPath;
		void coder.rows;
		void coder.view;
		void coder.sessions;
		performance.mark('moon:coderPanel.update');
	});

	// Auto-scroll the transcript when new rows land — but only
	// when the user is *already* parked at (or close to) the
	// bottom. If they scrolled up to look at an earlier message
	// or tool result, we leave their viewport alone instead of
	// yanking them back down on every fresh tool call. Coming
	// back to the bottom (manually scrolling there) re-arms the
	// auto-follow on the next row.
	//
	// Both `stickyBottom` and `lastRowCount` are plain `let`s,
	// not `$state`: nothing else reacts to them, and the effect
	// below should re-run only when *rows* change, not every
	// time the user drags the scrollbar.
	let stickyBottom = true;
	let lastRowCount = 0;
	const STICKY_BOTTOM_THRESHOLD_PX = 24;

	// Windowed (tail) transcript rendering. We only mount the last
	// `visibleCount` rows of `coder.rows`; older rows live in the
	// store but never hit the DOM until the user scrolls up far
	// enough to ask for them. This is the "load more as you scroll
	// up" virtualization: the common case (parked at the bottom
	// watching the latest turn stream in) renders a small bounded
	// slice no matter how many thousand rows the session holds, so
	// opening a giant session paints a screenful instead of the
	// whole history.
	//
	// We deliberately avoid fixed-height / measured-height
	// virtualization: coder rows are wildly variable in height
	// (a one-line tool summary vs. a 200-line read_file body vs. a
	// long markdown reply) and stream their height in over time,
	// so an offset-math windower would need constant re-measurement
	// and would fight the streaming layout. A trailing window
	// sidesteps all of that — no spacer divs, no height cache, and
	// streaming / sticky-bottom keep working unchanged because new
	// rows always land inside the window.
	// A transcript child that's an actual row (not the "Load older" /
	// "Load newer" pills or the "Jump to latest" button) — i.e. a
	// valid scroll anchor. Hoisted to script top-level so it isn't
	// recreated per `edgeRowEl` call.
	function isAnchorRow(child: Element): child is HTMLElement {
		return (
			child instanceof HTMLElement &&
			!child.classList.contains('load-older') &&
			!child.classList.contains('jump-latest')
		);
	}

	const INITIAL_WINDOW = 50;
	const WINDOW_GROW_STEP = 50;
	// Hard cap on how many rows the window ever mounts. Once a
	// scroll-up grow would push past this, the window stops growing
	// and instead *slides*: it drops `WINDOW_GROW_STEP` rows off the
	// bottom (off-screen while the user reads history) for each
	// chunk it pulls in at the top. So the DOM never holds more than
	// ~`WINDOW_MAX` rows no matter how far the user scrolls back.
	const WINDOW_MAX = 300;
	// Grow the window when the user scrolls within this many px of
	// the top — far enough ahead that the older rows are usually
	// mounted before they scroll into view.
	const LOAD_MORE_THRESHOLD_PX = 600;

	// Floating window over `coder.rows`, both edges tracked as
	// offsets from the *end* of the list so the bottom edge can
	// detach from the live tail:
	//   - `visibleCount`: how many rows the window spans (≤ WINDOW_MAX).
	//   - `bottomClip`:   how many rows are clipped off the bottom
	//                     (0 = window's bottom is the live tail).
	// `windowEnd = len - bottomClip`, `windowStart = windowEnd -
	// visibleCount`. When `bottomClip === 0` the window is anchored
	// to the tail and the streaming / sticky-bottom logic below runs
	// exactly as it did for the old tail-only window.
	let visibleCount = $state(INITIAL_WINDOW);
	let bottomClip = $state(0);

	const windowEnd = $derived(Math.max(0, coder.rows.length - bottomClip));
	const windowStart = $derived(Math.max(0, windowEnd - visibleCount));
	const windowedRows = $derived(coder.rows.slice(windowStart, windowEnd));
	const hiddenAbove = $derived(windowStart);
	const hiddenBelow = $derived(coder.rows.length - windowEnd);

	function onTranscriptScroll(): void {
		if (!scrollEl) {
			return;
		}
		const distance = scrollEl.scrollHeight - scrollEl.scrollTop - scrollEl.clientHeight;
		// Only treat the bottom as "sticky" when the window is
		// actually anchored to the live tail; a detached window
		// scrolled to its own bottom edge is not at the latest row.
		stickyBottom = bottomClip === 0 && distance <= STICKY_BOTTOM_THRESHOLD_PX;
		if (applyingAnchor) {
			// This scroll event is our own anchor-compensation write,
			// not a user gesture — don't let it trigger another grow.
			return;
		}
		// Near the top with older rows still hidden: pull more
		// history in. `captureScrollAnchor` pins the first rendered
		// row so the viewport doesn't lurch when the slice changes
		// (whether we grow or slide). Likewise near the bottom with
		// newer rows clipped (a detached window): pull newer rows in
		// as the user scrolls down toward them.
		if (scrollEl.scrollTop <= LOAD_MORE_THRESHOLD_PX && hiddenAbove > 0) {
			captureScrollAnchor();
			growWindowUp();
		} else if (distance <= LOAD_MORE_THRESHOLD_PX && hiddenBelow > 0) {
			captureScrollAnchor('last');
			growWindowDown();
		}
	}

	/** Pull `WINDOW_GROW_STEP` older rows into the window. Below the
	 *  `WINDOW_MAX` cap this just grows the span; at the cap it
	 *  slides — the same number of rows are dropped off the bottom
	 *  (which is off-screen while the user is up in history), so the
	 *  mounted row count stays bounded. Either way the top edge
	 *  moves up by one step, and the caller's scroll anchor keeps
	 *  the viewport steady. */
	function growWindowUp(): void {
		if (hiddenAbove <= 0) {
			return;
		}
		const step = Math.min(WINDOW_GROW_STEP, hiddenAbove);
		if (visibleCount + step <= WINDOW_MAX) {
			visibleCount += step;
			return;
		}
		// At the cap: keep the span, drop `step` off the bottom.
		visibleCount = WINDOW_MAX;
		bottomClip += step;
	}

	/** Mirror of `growWindowUp` for the bottom edge: pull
	 *  `WINDOW_GROW_STEP` newer rows into the window by un-clipping
	 *  the bottom. Below the cap it extends the span downward; at the
	 *  cap it slides — dropping the same number off the (off-screen)
	 *  top so the mounted row count stays bounded. The caller's
	 *  element anchor keeps the viewport steady either way. */
	function growWindowDown(): void {
		if (hiddenBelow <= 0) {
			return;
		}
		const step = Math.min(WINDOW_GROW_STEP, hiddenBelow);
		bottomClip -= step;
		if (visibleCount + step > WINDOW_MAX) {
			visibleCount = WINDOW_MAX;
		} else {
			visibleCount += step;
		}
	}

	/** Snap the window back to the live tail and scroll to the
	 *  bottom. The escape hatch from a detached (scrolled-up,
	 *  bottom-clipped) window — surfaced as the "Jump to latest"
	 *  button whenever `hiddenBelow > 0`. */
	function jumpToLatest(): void {
		bottomClip = 0;
		visibleCount = INITIAL_WINDOW;
		stickyBottom = true;
		pendingAnchorNode = null;
		void tick().then(() => {
			if (scrollEl) {
				scrollEl.scrollTop = scrollEl.scrollHeight;
			}
		});
	}

	// Scroll anchoring for a window slide. A grow prepends rows at
	// the top (and, at the cap, drops rows off the bottom), both of
	// which shift the scroll offset of everything the user is
	// looking at. Total-height math only works for a pure prepend;
	// the cap-slide also removes height below the viewport, so we
	// anchor on a concrete DOM node instead: capture the first
	// rendered row element + its viewport-relative top before the
	// change, then after the DOM settles nudge `scrollTop` by how far
	// that same (keyed, still-mounted) node moved. Correct whether
	// rows were added above, removed below, or both.
	let pendingAnchorNode: HTMLElement | null = null;
	let pendingAnchorNodeTop = 0;
	// Set while we apply the post-grow anchor's programmatic
	// `scrollTop` write. That write fires `onscroll`, which would
	// otherwise see the still-near-an-edge position and grow again,
	// cascading the whole history in one gesture. The guard makes
	// the synthetic scroll a no-op for the grow logic (sticky-bottom
	// tracking still updates).
	let applyingAnchor = false;

	/** A rendered transcript row element at the requested edge,
	 *  skipping the non-row chrome (the "Load older" / "Load newer"
	 *  pills and the "Jump to latest" button). Used as the scroll
	 *  anchor: we pin a row that's *stable* across the upcoming
	 *  slice change — the first row when pulling history in at the
	 *  top, the last row when pulling newer rows in at the bottom —
	 *  so the anchor node never gets unmounted by a cap-slide. */
	function edgeRowEl(edge: 'first' | 'last'): HTMLElement | null {
		if (!scrollEl) {
			return null;
		}
		const children = Array.from(scrollEl.children).filter(isAnchorRow);
		if (children.length === 0) {
			return null;
		}
		return edge === 'first' ? children[0]! : children[children.length - 1]!;
	}

	function captureScrollAnchor(edge: 'first' | 'last' = 'first'): void {
		const el = edgeRowEl(edge);
		if (el && scrollEl) {
			pendingAnchorNode = el;
			pendingAnchorNodeTop = el.getBoundingClientRect().top - scrollEl.getBoundingClientRect().top;
		}
	}

	// Explicit "Load older" click. Same grow + scroll-anchor path
	// the scroll-up auto-grow takes, surfaced as a button at the
	// top of the window so the user can pull in history without
	// having to nudge the scrollbar to the very top.
	function loadOlderRows(): void {
		if (hiddenAbove <= 0) {
			return;
		}
		captureScrollAnchor();
		growWindowUp();
	}

	// Explicit "Load newer" click — mirror of "Load older" for a
	// detached window's bottom edge. Surfaced as a pill at the
	// bottom of the rendered rows so the user can reel the window
	// back toward the tail a chunk at a time (vs. "Jump to latest",
	// which snaps all the way).
	function loadNewerRows(): void {
		if (hiddenBelow <= 0) {
			return;
		}
		captureScrollAnchor('last');
		growWindowDown();
	}
	$effect(() => {
		// Re-run whenever the rendered slice changes size.
		void windowedRows.length;
		if (pendingAnchorNode === null) {
			return;
		}
		const node = pendingAnchorNode;
		const prevTop = pendingAnchorNodeTop;
		pendingAnchorNode = null;
		void tick().then(() => {
			if (!scrollEl || !node.isConnected) {
				return;
			}
			const newTop = node.getBoundingClientRect().top - scrollEl.getBoundingClientRect().top;
			const delta = newTop - prevTop;
			if (delta !== 0) {
				applyingAnchor = true;
				scrollEl.scrollTop += delta;
				// Clear after the synthetic scroll event has been
				// dispatched + handled. A microtask is enough — the
				// `scroll` event fires synchronously on the assignment
				// in WebKit, but rAF is the safe lower bound.
				requestAnimationFrame(() => {
					applyingAnchor = false;
				});
			}
		});
	});

	// Collapse the render window back to the tail whenever the
	// mounted session changes (session swap, folder switch, sub-
	// agent pop-out). A freshly opened session should paint a
	// screenful, not inherit a previous session's scrolled-open
	// window — and the swap can grow the row list (so the
	// row-count-shrink reset above wouldn't catch it). Reading the
	// session id + view registers both as deps.
	let lastWindowSessionKey = '';
	$effect(() => {
		const key = `${coder.view}:${coder.activeSession?.id ?? ''}:${coder.viewSubagentId ?? ''}`;
		if (key === lastWindowSessionKey) {
			return;
		}
		lastWindowSessionKey = key;
		visibleCount = INITIAL_WINDOW;
		bottomClip = 0;
		pendingAnchorNode = null;
	});

	// Streaming-growth signal. The reducer mutates rows in place
	// (`row.text += delta`, `row.result = …`) rather than
	// reassigning `coder.rows`, so an append-only dep like
	// `coder.rows.length` does NOT re-run while a turn streams its
	// text/tool output in. Reading the last row's size-bearing
	// fields here gives the auto-scroll effect a dep that ticks on
	// every delta, so a sticky-bottom viewport keeps following the
	// growing last bubble. Cheap: one row, a couple of `.length`
	// reads.
	const tailGrowth = $derived.by(() => {
		const rows = coder.rows;
		const last = rows[rows.length - 1];
		if (last === undefined) {
			return 0;
		}
		if (last.kind === 'assistant') {
			return last.text.length + last.thinking.length;
		}
		if (last.kind === 'tool') {
			return last.hasResult ? 1 : 0;
		}
		return 0;
	});

	// Sub-agent activity signal. A `task` tool row renders an
	// inline collapsed card that gets *inserted* under the tool row
	// when `subagent_spawned` lands, then mutates over the
	// sub-agent's life (status pip flips on finish, token footer
	// ticks, result preview fills in). Every one of those changes
	// the card's rendered height, and *none* of them touch
	// `coder.rows`, so `tailGrowth` above stays flat and the
	// auto-scroll effect never re-runs — the viewport stalls the
	// moment a sub-agent launches.
	//
	// The dominant case is the spawn itself: a fresh summary's
	// numeric fields (`tokensUsedEstimate`, preview length) are all
	// zero, so a value-only digest wouldn't change and the effect's
	// `$derived` dep wouldn't fire. We therefore mix the summary
	// *count* and a per-status tick into the digest, so a brand-new
	// (all-zero) card still bumps the signal. Cheap: a handful of
	// summaries, a couple of adds each.
	const subagentGrowth = $derived.by(() => {
		const summaries = coder.subagentSummaries;
		let digest = summaries.size;
		for (const summary of summaries.values()) {
			digest += summary.tokensUsedEstimate + (summary.resultPreview?.length ?? 0);
			if (summary.status !== 'running') {
				digest += 1;
			}
		}
		return digest;
	});

	$effect(() => {
		const count = coder.rows.length;
		// Register the streaming-growth deps so this effect re-runs
		// on in-place text/result mutation and on sub-agent card
		// growth, not just on row append.
		void tailGrowth;
		void subagentGrowth;
		if (count < lastRowCount) {
			// Conversation reset: folder switch, sub-agent → main
			// pop, or session swap shrinks the row list. Re-arm
			// sticky-bottom so the next streamed message in the
			// new context still auto-follows. Without this the
			// flag stays "false" from a previous-session
			// scroll-up and the new conversation never anchors.
			stickyBottom = true;
			// Drop remembered tool-row expansions so a freshly
			// loaded transcript starts with every body cold
			// (collapsed = unmounted). Row ids are unique per
			// session, so a stale id would never match anyway,
			// but clearing keeps the set from growing unbounded
			// across many session swaps.
			openedToolRows.clear();
			// A shrink within the same session (revert-to-message,
			// sub-agent → main pop) invalidates the floating
			// window's offsets — snap it back to the live tail so
			// we don't end up clipping past the new, shorter list.
			visibleCount = INITIAL_WINDOW;
			bottomClip = 0;
			pendingAnchorNode = null;
		}
		const appended = count - lastRowCount;
		lastRowCount = count;
		// Re-confirm sticky-bottom against the *live* scroll
		// position rather than the cached flag. Streaming a turn in
		// place grows the last bubble's height without firing a
		// scroll event, so `stickyBottom` (only updated by
		// `onscroll`) can read stale-false while the user is in fact
		// still pinned to the bottom. A stale-false read here used
		// to bump `bottomClip` on the next append — detaching the
		// window and popping the "Jump to latest" button even though
		// the user never scrolled away. The live measurement closes
		// that race.
		if (scrollEl) {
			const distance = scrollEl.scrollHeight - scrollEl.scrollTop - scrollEl.clientHeight;
			if (bottomClip === 0 && distance <= STICKY_BOTTOM_THRESHOLD_PX) {
				stickyBottom = true;
			}
		}
		if (!stickyBottom) {
			// The user is parked up in history (a steer sent
			// mid-turn, then scrolled back to read; or a detached,
			// bottom-clipped window). New rows land at the tail.
			// We pin the *visible* rows in place by clipping the
			// new arrivals off the bottom: `windowEnd = len -
			// bottomClip` stays fixed, so nothing the user is
			// looking at moves, the mounted row count stays
			// bounded, and the "Jump to latest" button appears
			// (because `hiddenBelow` is now > 0).
			if (appended > 0) {
				bottomClip += appended;
			}
			return;
		}
		void tick().then(() => {
			if (scrollEl && stickyBottom) {
				scrollEl.scrollTop = scrollEl.scrollHeight;
			}
		});
	});

	// `@`-mention autocomplete state. `mention` is non-null while
	// the caret sits in an active `@token`; the menu mounts whenever
	// it is, and unmounts the moment the user types a space, deletes
	// the `@`, blurs, or hits Esc. `mentionResults` is the latest
	// debounced response from the file-search backend (same source
	// as the command palette's file mode), so picking a match feels
	// identical to `Ctrl+P` minus the modal.
	type MentionState = {
		// Byte (UTF-16 code-unit) offsets into the textarea value:
		// `start` points at the `@`, `end` at the caret. The slice
		// `[start, end)` is what gets replaced when the user picks.
		start: number;
		end: number;
		query: string;
	};
	let mention = $state<MentionState | null>(null);
	let mentionResults = $state<FileSearchResult[]>([]);
	let mentionSelected = $state(0);
	let mentionLoading = $state(false);
	let mentionDebounce: ReturnType<typeof setTimeout> | null = null;
	let mentionRequestId = 0;

	function closeMention(): void {
		mention = null;
		mentionResults = [];
		mentionSelected = 0;
		mentionLoading = false;
		if (mentionDebounce !== null) {
			clearTimeout(mentionDebounce);
			mentionDebounce = null;
		}
	}

	/** Walk back from the caret to find an active `@`-mention. A
	 *  mention is "active" when the nearest `@` is preceded by
	 *  whitespace (or the start of the field) and everything from
	 *  it up to the caret is non-whitespace + non-`@`. That rules
	 *  out email addresses (`foo@bar.com`) and the existing
	 *  `@path:N-M` chip tokens (which already carry a `:` we treat
	 *  as a "this is a fully-formed token, no menu" signal). */
	function detectMention(value: string, caret: number): MentionState | null {
		// Scan backwards for `@`, bailing on whitespace.
		let i = caret;
		while (i > 0) {
			const ch = value[i - 1];
			if (ch === undefined) {
				break;
			}
			if (/\s/.test(ch)) {
				return null;
			}
			i -= 1;
			if (value[i] === '@') {
				break;
			}
		}
		if (i === caret || value[i] !== '@') {
			return null;
		}
		// `@` must be at start-of-field or follow whitespace —
		// otherwise we're in the middle of a word (email, etc.).
		if (i > 0) {
			const prev = value[i - 1];
			if (prev !== undefined && !/\s/.test(prev)) {
				return null;
			}
		}
		const query = value.slice(i + 1, caret);
		// `path:N` shapes are pre-formed chip tokens — let them be.
		if (query.includes(':')) {
			return null;
		}
		return { start: i, end: caret, query };
	}

	function updateMention(): void {
		if (!composer) {
			closeMention();
			return;
		}
		const state = detectMention(composer.value, composer.selectionStart ?? composer.value.length);
		if (state === null) {
			closeMention();
			return;
		}
		const prev = mention;
		mention = state;
		if (prev === null || prev.query !== state.query) {
			mentionSelected = 0;
			void fetchMentionResults(state.query);
		}
	}

	async function fetchMentionResults(query: string): Promise<void> {
		// Bare `@` shows an empty menu with a "type to filter" hint —
		// no backend round-trip until there's at least one query
		// character. The backend treats empty queries as a no-op
		// anyway (`search_files` short-circuits on `query.trim()`).
		if (query.length === 0) {
			mentionResults = [];
			mentionLoading = false;
			return;
		}
		if (mentionDebounce !== null) {
			clearTimeout(mentionDebounce);
		}
		mentionLoading = true;
		const requestId = ++mentionRequestId;
		mentionDebounce = setTimeout(() => {
			mentionDebounce = null;
			void (async () => {
				try {
					const hits = await ipc.search.files({ query, limit: 8 });
					// Drop stale responses: the user kept typing and a
					// newer request is in flight, or they dismissed the
					// menu entirely.
					if (requestId !== mentionRequestId || mention === null) {
						return;
					}
					mentionResults = hits;
					mentionSelected = 0;
				} catch (err) {
					frontendLog('moon-ide', 'info', `coder mention: search failed: ${formatError(err)}`);
					if (requestId === mentionRequestId) {
						mentionResults = [];
					}
				} finally {
					if (requestId === mentionRequestId) {
						mentionLoading = false;
					}
				}
			})();
		}, 50);
	}

	function pickMention(index: number): void {
		const state = mention;
		const hit = mentionResults[index];
		if (state === null || hit === undefined) {
			return;
		}
		closeMention();
		coder.addAttachmentFromMention({
			path: hit.path,
			rangeStart: state.start,
			rangeEnd: state.end,
		});
	}

	// Pull focus into the composer whenever the store bumps its
	// focus tick (e.g. Ctrl+L from the editor pushes a selection
	// onto `coder.attachments` and wants the user typing
	// straight away). Reads the count to register the dep; the
	// `tick()` defers until after the panel re-renders the
	// chips, otherwise the focus call could race the textarea
	// being remounted.
	let lastFocusTick = $state(0);
	$effect(() => {
		const t = coder.composerFocusTick;
		if (t === lastFocusTick) {
			return;
		}
		lastFocusTick = t;
		void tick().then(() => {
			composer?.focus();
		});
	});

	async function onComposerKey(event: KeyboardEvent) {
		// Enter sends; Shift+Enter inserts a newline. Esc aborts the
		// active turn (matches the panel header's stop button).
		// ArrowUp on an empty composer (no modifiers) pulls the
		// most recent queued steer back into the draft so the user
		// can edit it before it lands in the chat — only fires
		// when there's actually something queued, so a plain
		// empty composer still falls through to the textarea's
		// default no-op. Ctrl+Z / Ctrl+Shift+Z / Ctrl+Y are wired
		// by the `use:textInputUndo` action on the textarea below.
		//
		// `@`-mention menu (when mounted) intercepts Arrow / Enter /
		// Tab / Escape *before* the regular composer bindings, so
		// the user can drive the picker without those keys firing
		// their default composer behaviour. Escape dismisses the
		// menu only — it doesn't abort the turn at the same time.
		if (mention !== null && mentionResults.length > 0) {
			if (event.key === 'ArrowDown') {
				event.preventDefault();
				mentionSelected = (mentionSelected + 1) % mentionResults.length;
				return;
			}
			if (event.key === 'ArrowUp') {
				event.preventDefault();
				mentionSelected = (mentionSelected - 1 + mentionResults.length) % mentionResults.length;
				return;
			}
			if (event.key === 'Enter' || event.key === 'Tab') {
				event.preventDefault();
				pickMention(mentionSelected);
				return;
			}
		}
		if (mention !== null && event.key === 'Escape') {
			event.preventDefault();
			closeMention();
			return;
		}
		if (event.key === 'Escape' && coder.busy) {
			event.preventDefault();
			await coder.abort();
			return;
		}
		if (
			event.key === 'ArrowUp' &&
			!event.shiftKey &&
			!event.ctrlKey &&
			!event.altKey &&
			!event.metaKey &&
			composer !== undefined &&
			composer.value.length === 0 &&
			coder.hasQueuedSteer
		) {
			event.preventDefault();
			await coder.unqueueLatestSteer();
			return;
		}
		if (event.key === 'Enter' && !event.shiftKey && !event.ctrlKey && !event.metaKey) {
			event.preventDefault();
			await coder.send();
		}
		// Atomic attachment tokens: a Backspace / Delete that would
		// chip away at an `@`-mention token (a selection / terminal
		// chip's inline reference) instead deletes the whole token
		// in one stroke, so the reference behaves like a single
		// block. Only fires on a collapsed caret with no modifiers
		// — a range selection or a Ctrl/Alt-modified delete keeps
		// its native behaviour. The resulting `input` event runs
		// the usual sync, which drops the now-orphaned chip too.
		if (
			(event.key === 'Backspace' || event.key === 'Delete') &&
			!event.ctrlKey &&
			!event.altKey &&
			!event.metaKey &&
			composer !== undefined &&
			composer.selectionStart === composer.selectionEnd
		) {
			const span = tokenSpanAtCaret(composer.value, composer.selectionStart, event.key === 'Backspace');
			if (span !== null) {
				event.preventDefault();
				composer.setSelectionRange(span.start, span.end);
				// `execCommand('delete')` keeps the native undo
				// stack intact and fires `input`, so `onComposerInput`
				// re-syncs the draft + chip strip from here.
				const ok = document.execCommand('delete');
				if (!ok) {
					const next = composer.value.slice(0, span.start) + composer.value.slice(span.end);
					composer.value = next;
					coder.draft = next;
					coder.syncAttachmentsToDraft(next);
					composer.setSelectionRange(span.start, span.start);
				}
			}
		}
	}

	/** Find the attachment token the caret sits in/adjacent to so a
	 *  single Backspace / Delete can wipe it whole. Returns the
	 *  `[start, end)` span of the matched token, or `null` when the
	 *  caret isn't poised to chip into one. For Backspace the caret
	 *  must be just after or inside the token (so the key would
	 *  otherwise eat the last char); for Delete it must be at or
	 *  inside the start (so the key would otherwise eat the first
	 *  char). Image attachments carry no token and are skipped. */
	function tokenSpanAtCaret(value: string, caret: number, backspace: boolean): { start: number; end: number } | null {
		for (const att of coder.attachments) {
			if (att.kind === 'image') {
				continue;
			}
			const { token } = att;
			let from = value.indexOf(token);
			while (from !== -1) {
				const to = from + token.length;
				const hit = backspace ? caret > from && caret <= to : caret >= from && caret < to;
				if (hit) {
					return { start: from, end: to };
				}
				from = value.indexOf(token, from + 1);
			}
		}
		return null;
	}

	function onComposerInput(event: Event): void {
		const ta = event.currentTarget as HTMLTextAreaElement;
		coder.draft = ta.value;
		// Editing the prose mirrors back onto the chip strip:
		// breaking (or deleting) a selection / terminal chip's
		// inline token drops the chip, so a reference behaves like
		// a chip in both directions.
		coder.syncAttachmentsToDraft(ta.value);
		updateMention();
	}

	// Caret-only moves (arrow keys, mouse clicks) don't fire
	// `input`, but they do change whether the caret sits inside an
	// `@`-mention. `selectionchange` lives on the document and is
	// the only event that fires reliably for both gestures across
	// our webview targets; we filter to events targeting the
	// composer so we don't react to selection changes elsewhere in
	// the page (the editor, the file tree, …).
	$effect(() => {
		function onSelectionChange(): void {
			if (composer && document.activeElement === composer) {
				updateMention();
			}
		}
		document.addEventListener('selectionchange', onSelectionChange);
		return () => document.removeEventListener('selectionchange', onSelectionChange);
	});

	function onComposerBlur(): void {
		// Close the menu when focus leaves the composer — clicking
		// a menu row is handled by `onmousedown` before the blur
		// fires, so the pick lands first.
		closeMention();
	}

	/** Intercept clipboard pastes so we can pull image blobs into
	 *  the chip strip instead of letting them fall through to the
	 *  textarea (which would either drop them or, on some
	 *  browsers, paste a stringified `[object File]` placeholder).
	 *  Plain-text pastes pass through untouched — we only call
	 *  `preventDefault` when we actually consumed at least one
	 *  image from the payload. Mixed payloads (an image plus a
	 *  text representation, common when copying from screenshot
	 *  apps) attach the image *and* let the text portion paste,
	 *  so the user can keep typing around the image they just
	 *  dropped in.
	 *
	 *  Looking in three places, in order, because WebKitGTK's
	 *  clipboard layer is inconsistent about which one a
	 *  screenshot-tool paste lands in:
	 *  1. `clipboardData.items` with `kind === 'file'` — the
	 *     standard path Chromium / Safari macOS use.
	 *  2. `clipboardData.files` — WebKit on some Linux distros
	 *     populates this list while leaving `items[*].kind` set
	 *     to `'string'`.
	 *  3. `clipboardData.items` with any MIME starting `image/` —
	 *     fallback for the same WebKit edge case where `kind` is
	 *     `'string'` but `type` is `image/png` and `getAsFile()`
	 *     still works. */
	async function onComposerPaste(event: ClipboardEvent): Promise<void> {
		const data = event.clipboardData;
		if (data === null) {
			frontendLog('moon-ide', 'info', 'composer paste: clipboardData is null');
			return;
		}
		const items = Array.from(data.items);
		const itemDescr = items.map((it) => `${it.kind}/${it.type || '<no-type>'}`).join(', ');
		const fileDescr = Array.from(data.files)
			.map((f) => `${f.type || '<no-type>'}/${f.size}B`)
			.join(', ');
		frontendLog(
			'moon-ide',
			'info',
			`composer paste: items=[${itemDescr}] files=[${fileDescr}] types=[${data.types.join(', ')}]`,
		);
		const blobs: File[] = [];
		for (const it of items) {
			if (it.type.startsWith('image/')) {
				const f = it.getAsFile();
				if (f !== null) {
					blobs.push(f);
				}
			}
		}
		if (blobs.length === 0) {
			for (const f of Array.from(data.files)) {
				if (f.type.startsWith('image/')) {
					blobs.push(f);
				}
			}
		}
		if (blobs.length === 0) {
			// The WebKitGTK image-clipboard workaround. WebKit
			// hands us a totally empty `ClipboardEvent` for many
			// image clipboards (screenshot tools, image apps),
			// even though the OS clipboard holds a picture; we
			// fall through to the Tauri-side `readImage` API
			// (arboard) for that case. Crucially we only do this
			// when the event itself is empty — if it carries
			// `text/plain` (or anything else) we let the textarea
			// handle it normally. Previously we always
			// `preventDefault`ed here, which silently ate every
			// text paste because the OS clipboard image read
			// returned null and the original text never landed.
			const eventEmpty = items.length === 0 && data.files.length === 0 && data.types.length === 0;
			if (!eventEmpty) {
				return;
			}
			event.preventDefault();
			const blob = await tryReadClipboardImage();
			if (blob === null) {
				frontendLog('moon-ide', 'info', 'composer paste: no image found in clipboard (web or tauri)');
				return;
			}
			const result = await coder.addImageAttachment(blob, 'pasted.png');
			if (!result.ok) {
				coder.rows = [
					...coder.rows,
					{
						kind: 'error',
						id: `local-${Date.now()}-${Math.random().toString(36).slice(2, 6)}`,
						text: `Couldn't attach image: ${result.reason}`,
					},
				];
			}
			return;
		}
		const hasText = items.some((it) => it.kind === 'string' && it.type === 'text/plain');
		if (!hasText) {
			event.preventDefault();
		}
		for (const blob of blobs) {
			const result = await coder.addImageAttachment(blob, blob.name);
			if (!result.ok) {
				coder.rows = [
					...coder.rows,
					{
						kind: 'error',
						id: `local-${Date.now()}-${Math.random().toString(36).slice(2, 6)}`,
						text: `Couldn't attach image: ${result.reason}`,
					},
				];
			}
		}
	}

	/** Tauri-side clipboard read that bypasses WebKitGTK. Returns
	 *  a `Blob` when the OS clipboard actually holds an image,
	 *  `null` otherwise (clipboard empty / text-only / read
	 *  failed). The plugin gives us raw RGBA bytes; we re-encode
	 *  to PNG via OffscreenCanvas because providers want a real
	 *  image MIME on the wire, not raw pixels. */
	async function tryReadClipboardImage(): Promise<Blob | null> {
		const image = await readImage().catch((err: unknown) => {
			// "no image in clipboard" is the common, expected
			// failure mode and not worth a louder log; we
			// already log "no image found in clipboard" above.
			frontendLog('moon-ide', 'info', `composer paste: tauri readImage failed: ${String(err)}`);
			return null;
		});
		if (image === null) {
			return null;
		}
		const size = await image.size();
		const rgba = await image.rgba();
		if (size.width === 0 || size.height === 0 || rgba.length === 0) {
			return null;
		}
		frontendLog(
			'moon-ide',
			'info',
			`composer paste: tauri readImage ${size.width}x${size.height} (${rgba.length}B rgba)`,
		);
		const canvas = new OffscreenCanvas(size.width, size.height);
		const ctx = canvas.getContext('2d');
		if (ctx === null) {
			return null;
		}
		const data = new ImageData(new Uint8ClampedArray(rgba), size.width, size.height);
		ctx.putImageData(data, 0, 0);
		return await canvas.convertToBlob({ type: 'image/png' });
	}

	// State → DOM sync for *external* draft writes only: a folder
	// switch (the bucket getter returns a different bucket's
	// draft), an attachment chip removal (`removeAttachment`
	// regex-replaces tokens out of the draft), or a post-send
	// clear. During plain typing the `oninput` handler above
	// keeps `coder.draft` and `composer.value` in lockstep, so
	// this effect's `value !== composer.value` check fails and
	// it does nothing — which is exactly what preserves the
	// textarea's native Ctrl+Z buffer. Going the other way (using
	// `bind:value`) made Svelte's binding effect periodically
	// reassign `composer.value`, and any JS write to a textarea's
	// `value` clears its native undo history; that's the bug
	// this whole approach side-steps.
	$effect(() => {
		const value = coder.draft;
		if (composer && composer.value !== value) {
			composer.value = value;
		}
	});

	async function onSignOut() {
		const ok = await confirm('Sign out of Hugging Face?', { title: 'Sign out', kind: 'warning' });
		if (ok) {
			await coder.signOut();
		}
	}

	function fmtArgs(value: unknown): string {
		if (value === null || value === undefined) {
			return '';
		}
		try {
			return JSON.stringify(value, null, 2);
		} catch {
			return String(value);
		}
	}

	/** Pull the typed shape of the `bash` tool's arguments — see
	 *  `crates/moon-coder/src/tools.rs`'s `BashArgs`. Returns
	 *  `null` when the args don't look like bash args, which lets
	 *  the renderer fall back to the JSON path. */
	function parseBashArgs(args: unknown): { cmd: string; timeoutMs: number | null } | null {
		if (typeof args !== 'object' || args === null) {
			return null;
		}
		const a = args as { cmd?: unknown; timeout_ms?: unknown };
		if (typeof a.cmd !== 'string') {
			return null;
		}
		const t = typeof a.timeout_ms === 'number' ? a.timeout_ms : null;
		return { cmd: a.cmd, timeoutMs: t };
	}

	/** Pull the typed shape of the `bash` tool's success result —
	 *  see `crates/moon-coder/src/tools.rs`'s `bash` `json!` block.
	 *  Returns `null` when the result was structured by an error
	 *  path (a string error, an unrelated object) so the renderer
	 *  drops to the JSON fallback for those edge cases. */
	function parseBashResult(result: unknown): {
		cmd: string | null;
		stdout: string;
		stderr: string;
		exitCode: number | null;
		target: string | null;
	} | null {
		if (typeof result !== 'object' || result === null) {
			return null;
		}
		const r = result as Record<string, unknown>;
		// Heuristic: the bash result always carries either
		// `stdout`, `stderr`, or `exit_code`. Anything missing
		// all three isn't bash-shaped — treat as JSON.
		if (typeof r.stdout !== 'string' && typeof r.stderr !== 'string' && typeof r.exit_code !== 'number') {
			return null;
		}
		return {
			cmd: typeof r.cmd === 'string' ? r.cmd : null,
			stdout: typeof r.stdout === 'string' ? r.stdout : '',
			stderr: typeof r.stderr === 'string' ? r.stderr : '',
			exitCode: typeof r.exit_code === 'number' ? r.exit_code : null,
			target: typeof r.target === 'string' ? r.target : null,
		};
	}

	/** Single-line hint shown next to the tool name in the
	 *  collapsed `<summary>` line — gives the user enough context
	 *  to recognise a tool call without expanding it. The shape is
	 *  the most identifying argument for each tool: path for the
	 *  file ones, the command for `bash`, the pattern / query for
	 *  search, the URL for `web_fetch`. Returns `null` for tool
	 *  names we don't have a hint shape for, and for malformed
	 *  args (the chip just won't render — the JSON-fallback body
	 *  carries the raw payload when expanded). The arg-key fallback
	 *  list (`path` / `file_path` / `file`) mirrors the per-tool
	 *  parsers in `ToolBody*.svelte` so a model that uses any of
	 *  the spellings still gets a chip.
	 *
	 *  We collapse the hint to the first line of the value so a
	 *  multi-line bash heredoc doesn't blow up the row height; the
	 *  full payload remains visible in the expanded body. */
	function toolHint(name: string, args: unknown): string | null {
		if (typeof args !== 'object' || args === null) {
			return null;
		}
		const o = args as Record<string, unknown>;
		const pickPath = (): string | null => {
			const candidate = o.path ?? o.file_path ?? o.file;
			return typeof candidate === 'string' && candidate.length > 0 ? candidate : null;
		};
		switch (name) {
			case 'bash': {
				return typeof o.cmd === 'string' ? firstLine(o.cmd) : null;
			}
			case 'read_file':
			case 'write_file':
			case 'edit_file':
			case 'list_dir': {
				return pickPath();
			}
			case 'grep': {
				return typeof o.pattern === 'string' ? firstLine(o.pattern) : null;
			}
			case 'web_search': {
				return typeof o.query === 'string' ? firstLine(o.query) : null;
			}
			case 'web_fetch': {
				return typeof o.url === 'string' ? o.url : null;
			}
			case 'todo_write': {
				return todoWriteHint(o);
			}
			case 'task': {
				const folder = typeof o.folder === 'string' && o.folder.length > 0 ? o.folder : null;
				const mode = typeof o.mode === 'string' && o.mode.length > 0 ? o.mode : 'agent';
				const taskText = typeof o.task === 'string' ? firstLine(o.task) : null;
				const head = folder !== null ? `${folder} · ${mode}` : mode;
				return taskText !== null ? `${head} — ${taskText}` : head;
			}
			default:
				return null;
		}
	}

	/** Hint preview for `todo_write` row summaries. Reads the
	 *  args (the model's proposed list at call time): if any item
	 *  is `in_progress`, show its content prefixed with `→` so
	 *  the user sees what the agent is committing to right now;
	 *  otherwise fall back to a `M / N done` count summary. The
	 *  header pill is the always-visible at-rest indicator; this
	 *  one's per-row context. Returns `null` when args don't
	 *  parse so the chip just doesn't render. */
	function todoWriteHint(o: Record<string, unknown>): string | null {
		const todos = o.todos;
		if (!Array.isArray(todos) || todos.length === 0) {
			return null;
		}
		let inProgress: string | null = null;
		let done = 0;
		for (const item of todos) {
			if (typeof item !== 'object' || item === null) {
				continue;
			}
			const t = item as { content?: unknown; status?: unknown };
			if (t.status === 'in_progress' && typeof t.content === 'string' && inProgress === null) {
				inProgress = t.content;
			} else if (t.status === 'completed' || t.status === 'cancelled') {
				done += 1;
			}
		}
		if (inProgress !== null) {
			return `→ ${firstLine(inProgress) ?? inProgress}`;
		}
		return `${done} / ${todos.length} done`;
	}

	function firstLine(s: string): string | null {
		const trimmed = s.replace(/^\s+/, '');
		if (trimmed.length === 0) {
			return null;
		}
		const nl = trimmed.indexOf('\n');
		if (nl === -1) {
			return trimmed;
		}
		// Trailing `…` so the user knows the value spans more than
		// one line; the expanded body shows the full text.
		return `${trimmed.slice(0, nl).trimEnd()} …`;
	}

	// Live tick fed into running tool rows so their elapsed-time
	// readout (`running… (Xs)`) advances. One shared interval per
	// panel — every tool row reads the same `nowTick` and computes
	// its own elapsed locally. We pause the interval when no tool
	// is running so an idle panel doesn't burn a wakeup per
	// second, and restart it the moment a fresh `tool_call`
	// arrives. 250ms keeps the readout feeling live without
	// turning into a stopwatch ("0.0 / 0.2 / 0.5 / 0.7…");
	// formatting clamps to one decimal so 4.2s reads cleanly even
	// when the underlying tick lands at 4.247s.
	let nowTick = $state(Date.now());
	const hasRunningTool = $derived(coder.rows.some((row) => row.kind === 'tool' && !row.hasResult));

	// Lazy tool-body rendering. Tool rows render collapsed by
	// default, but a `<details>` keeps its slotted children mounted
	// even while closed — so without this gate every `ToolBody*`
	// instance runs its grammar load + syntax-highlight pass on
	// first paint, even for rows the user never expands. On a long
	// session that fans out into hundreds of highlighter loads
	// during the initial render and stalls the panel for seconds
	// (the documented follow-up in test-plan 0076, rec'n 1).
	//
	// We render each tool body only once its `<details>` has been
	// opened at least once, tracked here by row id. The set is
	// sticky: re-collapsing a row keeps its body mounted (the work
	// is already paid for, and tearing it down would re-run the
	// highlighter on the next open). Cleared on session swap via
	// the row-count reset below so a fresh transcript starts cold.
	let openedToolRows = $state(new SvelteSet<string>());

	function onToolToggle(event: Event, rowId: string): void {
		const details = event.currentTarget;
		if (details instanceof HTMLDetailsElement && details.open) {
			openedToolRows.add(rowId);
		}
	}
	$effect(() => {
		if (!hasRunningTool) {
			return;
		}
		nowTick = Date.now();
		const handle = window.setInterval(() => {
			nowTick = Date.now();
		}, 250);
		return () => window.clearInterval(handle);
	});

	/** Format an elapsed duration in milliseconds for the tool row
	 *  summary line. Two display regimes:
	 *
	 *  - **Live (still running)**: sub-second values render as
	 *    "Xms" so a flash-fast `read_file` reads honestly; once
	 *    we cross 1s we switch to whole-second resolution
	 *    (`floor`) so the counter ticks "1s → 2s → 3s" rather
	 *    than chasing the 250ms sample boundary ("0.8 → 1.2 →
	 *    1.5"). Beyond a minute we go to "Mm SSs".
	 *  - **Final (`hasResult`)**: same shape, except sub-minute
	 *    values keep one decimal so "1.2s" / "12ms" — captures
	 *    the precise duration the user wants for spotting slow
	 *    tools after the fact.
	 */
	function fmtElapsed(ms: number, live: boolean): string {
		if (ms < 0) {
			return '0ms';
		}
		if (ms < 1000) {
			return `${Math.round(ms)}ms`;
		}
		if (ms < 60_000) {
			if (live) {
				return `${Math.floor(ms / 1000)}s`;
			}
			return `${(ms / 1000).toFixed(1)}s`;
		}
		const min = Math.floor(ms / 60_000);
		const sec = Math.floor((ms % 60_000) / 1000);
		return `${min}m ${sec.toString().padStart(2, '0')}s`;
	}

	async function onNewSession(): Promise<void> {
		await coder.newSession();
		// Land focus in the composer so a fresh session is one
		// keystroke away from being filled in.
		await tick();
		composer?.focus();
	}

	async function onPickSession(id: string): Promise<void> {
		await coder.openSession(id);
		await tick();
		composer?.focus();
	}

	async function onDeleteSession(event: MouseEvent, id: string, title: string): Promise<void> {
		// Stop the click from propagating into the row's "open"
		// button — without this, deleting a session would also
		// open it for a brief moment.
		event.stopPropagation();
		const ok = await confirm(`Delete session "${title || '(untitled)'}"? This cannot be undone.`, {
			title: 'Delete session',
			kind: 'warning',
		});
		if (!ok) {
			return;
		}
		await coder.deleteSession(id);
	}

	/** Revert the visible session back to just before the user
	 *  message with `rowId`, discarding it and everything after.
	 *  Confirmed because it's a destructive rewrite of the on-disk
	 *  transcript. */
	async function onRevertToMessage(rowId: string): Promise<void> {
		const ok = await confirm('Revert to this message? Everything after it will be permanently removed.', {
			title: 'Revert session',
			kind: 'warning',
		});
		if (!ok) {
			return;
		}
		await coder.revertToMessage(rowId, { resend: false });
	}

	/** Drop the user message with `rowId` (and everything after)
	 *  off the transcript and load its text back into the composer
	 *  so the user can tweak it and re-send. No confirm dialog: the
	 *  text isn't lost (it lands in the composer), and the gesture
	 *  reads as "edit", which a modal would make feel heavier than
	 *  it is. */
	async function onEditAndResend(rowId: string): Promise<void> {
		await coder.revertToMessage(rowId, { resend: true });
		await tick();
		composer?.focus();
	}

	/** Open a session's raw JSONL trace in the editor as a host-direct
	 *  file (same machinery as Ctrl+O for files outside the workspace).
	 *  Works for parent sessions and sub-agent ids alike — both live
	 *  under the active folder's slug on disk. The trace lives on the
	 *  *host* `XDG_DATA_HOME` even when the project is running in a
	 *  container, which is exactly what the host-direct file path
	 *  delivers.
	 *
	 *  Empty / never-persisted sessions surface as a flash via the
	 *  backend's `not found` error; the user can keep working. */
	async function onOpenTrace(event: MouseEvent | null, id: string): Promise<void> {
		event?.stopPropagation();
		let path: string;
		try {
			path = await ipc.coder.sessionJsonlPath(id);
		} catch (err) {
			workspace.flash(`Could not open trace: ${formatError(err)}`);
			return;
		}
		await workspace.openHostFile(path);
	}

	function baseName(path: string): string {
		const trimmed = path.replace(/\/+$/, '');
		const idx = trimmed.lastIndexOf('/');
		return idx >= 0 ? trimmed.slice(idx + 1) : trimmed;
	}

	// Copy an assistant message's raw markdown source to the
	// clipboard. Flips the button label to "Copied" / "Failed" for
	// ~1.2s so the user gets visible feedback inside a webview where
	// "did the clipboard actually take?" is otherwise invisible.
	// Failure surfaces as `Failed` rather than a flash because the
	// button itself is the affordance the user just clicked.
	async function onCopyAssistantMarkdown(event: MouseEvent, text: string): Promise<void> {
		event.stopPropagation();
		const button = event.currentTarget;
		if (!(button instanceof HTMLButtonElement)) {
			return;
		}
		let ok = false;
		try {
			await navigator.clipboard.writeText(text);
			ok = true;
		} catch {
			ok = false;
		}
		button.textContent = ok ? 'Copied' : 'Failed';
		window.setTimeout(() => {
			button.textContent = 'Copy markdown';
		}, 1200);
	}

	async function onOpenAttachment(attachment: { path: string; startLine: number }): Promise<void> {
		// Open the file and jump to the first line of the captured
		// range. We don't try to restore the original column / end
		// line — the chip is "show me the context I attached", not
		// a full re-selection gesture. `jumpTo` handles the open
		// + nav-history bookkeeping (same path Ctrl/Cmd-click
		// goto-definition takes), so Alt+Left after the chip click
		// returns to wherever the user was.
		await workspace.jumpTo(attachment.path, { line: attachment.startLine - 1, character: 0 });
	}

	type ParsedAttachment = { path: string; startLine: number; endLine: number };

	/** Pull the trailing `<context>...</context>` block out of a user
	 *  message and parse its `<code_selection>` children into clickable
	 *  references. The context block always sits at the very end of
	 *  the prompt (see `renderPromptWithAttachments`), preceded by
	 *  exactly two newlines — so we anchor the regex to `$` rather
	 *  than scanning the whole text. Malformed input falls through:
	 *  on a parse miss we just render the raw text and skip chip
	 *  rendering, never crash on edge cases (model echoing back
	 *  `<context>` in an answer, partial buffers during streaming,
	 *  etc.). */
	function parseUserPrompt(text: string): { prose: string; attachments: ParsedAttachment[] } {
		const match = text.match(/(?:\n\n)?<context>\n([\s\S]*?)\n<\/context>\s*$/);
		if (!match) {
			return { prose: text, attachments: [] };
		}
		const proseEnd = match.index ?? 0;
		const prose = text.slice(0, proseEnd);
		const inner = match[1] ?? '';
		const selectionRe = /<code_selection\s+path="([^"]*)"\s+lines="([^"]*)">/g;
		const attachments: ParsedAttachment[] = [];
		let m: RegExpExecArray | null;
		while ((m = selectionRe.exec(inner)) !== null) {
			const rawPath = m[1];
			const range = m[2];
			if (rawPath === undefined || range === undefined) {
				continue;
			}
			const path = unescapeXmlAttr(rawPath);
			const dash = range.indexOf('-');
			let startLine: number;
			let endLine: number;
			if (dash >= 0) {
				startLine = parseInt(range.slice(0, dash), 10);
				endLine = parseInt(range.slice(dash + 1), 10);
			} else {
				startLine = parseInt(range, 10);
				endLine = startLine;
			}
			if (Number.isFinite(startLine) && Number.isFinite(endLine) && startLine > 0) {
				attachments.push({ path, startLine, endLine });
			}
		}
		return { prose, attachments };
	}

	function unescapeXmlAttr(s: string): string {
		return s.replaceAll('&quot;', '"').replaceAll('&lt;', '<').replaceAll('&gt;', '>').replaceAll('&amp;', '&');
	}

	const RELATIVE_FORMATTER = new Intl.RelativeTimeFormat(undefined, { numeric: 'auto' });

	function formatRelative(ms: number): string {
		const diff = Date.now() - ms;
		// Coarse buckets — sessions usually span minutes to days,
		// not seconds. Mirrors the chat panel's "2m" / "3h" feel
		// without pulling in date-fns.
		const seconds = Math.round(diff / 1000);
		if (seconds < 60) {
			return RELATIVE_FORMATTER.format(-seconds, 'second');
		}
		const minutes = Math.round(seconds / 60);
		if (minutes < 60) {
			return RELATIVE_FORMATTER.format(-minutes, 'minute');
		}
		const hours = Math.round(minutes / 60);
		if (hours < 24) {
			return RELATIVE_FORMATTER.format(-hours, 'hour');
		}
		const days = Math.round(hours / 24);
		if (days < 30) {
			return RELATIVE_FORMATTER.format(-days, 'day');
		}
		const months = Math.round(days / 30);
		return RELATIVE_FORMATTER.format(-months, 'month');
	}
</script>

<div class="panel" data-region="coder">
	<header class="header">
		<div class="title">
			<span class="label">Coder</span>
			{#if coder.identity}
				<span class="who">{coder.identity.username}</span>
			{/if}
			{#if coder.bashTarget}
				<div class="target-wrap" bind:this={shellTargetWrap}>
					<button
						type="button"
						class="target"
						class:container={coder.bashTarget === 'container'}
						class:overridden={coder.forceHostOverride}
						title={coder.forceHostOverride
							? 'Forced to host mode — bash runs on the host, not the container. Click to change.'
							: coder.bashTarget === 'container'
								? 'bash and shell tools run inside the workspace container. Click to change.'
								: 'bash and shell tools run on the host machine. Click to change.'}
						aria-label="Shell target: {coder.bashTarget}{coder.forceHostOverride ? ' (forced)' : ''}"
						aria-haspopup="menu"
						aria-expanded={shellTargetOpen}
						onclick={() => (shellTargetOpen = !shellTargetOpen)}
					>
						<TerminalTargetIcon kind={coder.bashTarget} size={12} />
						{#if coder.forceHostOverride}
							<span class="override-dot" aria-hidden="true"></span>
						{/if}
					</button>
					{#if shellTargetOpen}
						<div class="target-popover" role="menu" aria-label="Shell target override">
							<button
								type="button"
								class="opt"
								class:selected={!coder.forceHostOverride}
								role="menuitemradio"
								aria-checked={!coder.forceHostOverride}
								onclick={() => selectShellTarget(false)}
							>
								<span class="opt-radio" aria-hidden="true"></span>
								<span class="opt-text">
									<span class="opt-title">Auto</span>
									<span class="opt-sub">
										Run bash in the workspace container when it's running.
										{#if !coder.forceHostOverride && coder.bashTarget}
											<em>Currently: {coder.bashTarget}.</em>
										{/if}
									</span>
								</span>
							</button>
							<button
								type="button"
								class="opt"
								class:selected={coder.forceHostOverride}
								role="menuitemradio"
								aria-checked={coder.forceHostOverride}
								onclick={() => selectShellTarget(true)}
							>
								<span class="opt-radio" aria-hidden="true"></span>
								<span class="opt-text">
									<span class="opt-title">Force host</span>
									<span class="opt-sub">
										Run bash on the host machine even while the workspace is containerized. For inspecting Docker, host
										networking, etc. Files are edited the same way either way.
									</span>
								</span>
							</button>
						</div>
					{/if}
				</div>
			{/if}
		</div>
		<div class="actions">
			<!-- At-a-glance pill for the agent's todo list: dominant
				 status glyph + `done / total` count. Hidden when the
				 list is empty; click expands a popover with the full
				 list. Sits to the left of the context ring so the
				 reading order is "what's the agent doing right now?"
				 then "how much room is left in the window?". -->
			<CoderTodoPill />
			<!-- Rolling context-window indicator: arc fills as the
				 next round-trip's prompt grows, ticks into warning /
				 danger before auto-compaction kicks in, and pulses
				 while a compaction summary is being written. -->
			<ContextRing usage={coder.tokenUsage} compaction={coder.compaction} />
			{#if coder.busy}
				<button type="button" class="stop" title="Stop turn (Esc)" onclick={() => coder.abort()}>stop</button>
			{/if}
			<!-- Swap the right-side slot from coder to chat. Same
				 affordance the chat panel has in the other
				 direction. -->
			<button
				type="button"
				class="icon"
				title="Switch to Chat"
				aria-label="Switch to Chat"
				onclick={() => slack.togglePanel()}
			>
				<ChatBubbleIcon />
			</button>
			{#if coder.signedIn}
				<button
					type="button"
					class="icon"
					class:active={coder.hubBucket !== null}
					title={coder.hubBucket
						? `HF trace sync: ${coder.hubBucket.namespace}/${coder.hubBucket.name}`
						: 'Connect HF trace sync'}
					aria-label="Hugging Face trace sync"
					onclick={() => (hubSettingsOpen = true)}
				>
					<CloudSyncIcon />
				</button>
				<button
					type="button"
					class="icon"
					title="Model settings"
					aria-label="Model settings"
					onclick={() => (modelSettingsOpen = true)}
				>
					<SettingsIcon />
				</button>
				<button type="button" class="icon" title="Sign out" aria-label="Sign out" onclick={onSignOut}>
					<SignOutIcon />
				</button>
			{/if}
		</div>
	</header>

	{#if !coder.signedIn}
		<div class="empty">
			<p class="empty-lede">Sign in with Hugging Face to use the AI coder.</p>
			<button type="button" class="primary" onclick={() => coder.startDeviceFlow()} disabled={coder.startingFlow}>
				{coder.startingFlow ? 'Requesting code…' : 'Sign in with Hugging Face'}
			</button>
			{#if coder.signInError && coder.deviceCode === null}
				<p class="error">{coder.signInError}</p>
			{/if}
		</div>
	{:else if coder.view === 'list'}
		<!-- Sessions list view (mirrors the Slack panel's "← Sessions"
			 affordance). Sticky header has the "+" button; the list
			 itself takes care of empty state. -->
		<div class="sessions">
			<header class="sessions-header">
				<span class="section-title">Sessions</span>
				<div class="header-actions">
					<button type="button" class="icon" onclick={onNewSession} title="New session" aria-label="New session">
						<PlusIcon />
					</button>
				</div>
			</header>
			{#if coder.sessions === null}
				<p class="hint">Loading sessions…</p>
			{:else if coder.sessions.length === 0}
				<p class="hint">
					No sessions yet. Click <strong>+</strong> above (or send a prompt) to start a fresh conversation.
				</p>
			{:else}
				<ul class="session-list">
					{#each coder.sessions as session (session.id)}
						{@const isVisible = coder.currentSession.activeSession?.id === session.id}
						{@const isRunning = coder.current.isSessionRunning(session.id)}
						{@const isFinished = !isRunning && coder.current.isSessionAttention(session.id)}
						<li class="session-row" class:active={isVisible} class:running={isRunning} class:finished={isFinished}>
							<button
								type="button"
								class="session-pick"
								onclick={() => onPickSession(session.id)}
								title={isRunning
									? 'Session is running — click to follow'
									: isFinished
										? 'Finished while you were away — click to open'
										: 'Open session'}
							>
								<div class="session-title">
									{#if isRunning}
										<span class="running-dot" aria-hidden="true"></span>
									{:else if isFinished}
										<span class="finished-dot" aria-hidden="true"></span>
									{/if}
									<span class="session-title-text">{session.title || '(untitled)'}</span>
								</div>
								<div class="session-meta">
									{#if isRunning}
										<span class="running-label">running…</span>
										<span class="session-meta-sep">·</span>
									{:else if isFinished}
										<span class="finished-label">finished</span>
										<span class="session-meta-sep">·</span>
									{/if}
									{formatRelative(session.updated_at_ms)}
								</div>
							</button>
							<button
								type="button"
								class="icon session-row-action"
								title="Open trace in editor"
								aria-label="Open trace in editor"
								onclick={(event) => onOpenTrace(event, session.id)}
							>
								<CodeIcon />
							</button>
							{#if coder.hubBucket}
								{@const phase = hubRowState(session.id)}
								<button
									type="button"
									class="icon session-row-action hub-action"
									class:syncing={phase === 'syncing'}
									class:synced={phase === 'synced'}
									class:failed={phase === 'failed'}
									title={hubRowTitle(session.id)}
									aria-label={hubRowTitle(session.id)}
									onclick={(event) => onUploadSession(event, session.id)}
									disabled={phase === 'syncing'}
								>
									<CloudUploadIcon />
								</button>
								{#if coder.hubBucket.uploaded[session.id]}
									<button
										type="button"
										class="icon session-row-action"
										title="Open trace on Hugging Face (Alt-click to copy URL)"
										aria-label="Open trace on Hugging Face"
										onclick={(event) => onOpenTraceOnHub(event, session.id)}
									>
										<ExternalLinkIcon />
									</button>
								{/if}
							{/if}
							<button
								type="button"
								class="icon session-row-action"
								title="Delete session"
								aria-label="Delete session"
								onclick={(event) => onDeleteSession(event, session.id, session.title)}
							>
								<TrashIcon />
							</button>
						</li>
					{/each}
				</ul>
			{/if}
		</div>
	{:else if coder.view === 'session'}
		<!-- Sticky in-session header: a small back-to-list affordance,
			 the session title (centre, prominent), and the "+ new"
			 button. Both buttons inherit `.icon`'s muted styling so
			 the title stays the visual focus — this strip is for
			 navigation, not headline content. -->
		<header class="session-bar">
			<button
				type="button"
				class="icon"
				onclick={() => coder.showSessionsList()}
				title="Back to sessions"
				aria-label="Back to sessions"
			>
				<ListIcon />
			</button>
			<span class="session-bar-title" title={coder.activeSession?.title ?? ''}>
				{coder.activeSession?.title ?? 'New session'}
			</span>
			{#if coder.activeSession}
				<button
					type="button"
					class="icon"
					onclick={() => onOpenTrace(null, coder.activeSession!.id)}
					title="Open trace in editor"
					aria-label="Open trace in editor"
				>
					<CodeIcon />
				</button>
				{#if coder.hubBucket?.uploaded[coder.activeSession.id]}
					<button
						type="button"
						class="icon"
						onclick={(event) => onOpenTraceOnHub(event, coder.activeSession!.id)}
						title="Open trace on Hugging Face (Alt-click to copy URL)"
						aria-label="Open trace on Hugging Face"
					>
						<ExternalLinkIcon />
					</button>
				{/if}
			{/if}
			<button type="button" class="icon" onclick={onNewSession} title="New session" aria-label="New session">
				<PlusIcon />
			</button>
		</header>
		<div class="transcript" bind:this={scrollEl} onscroll={onTranscriptScroll}>
			{#if coder.rows.length === 0}
				<p class="hint">
					Send a prompt to start. The agent can read files, list directories, search, and run shell commands.
				</p>
			{/if}
			{#if hiddenAbove > 0}
				<!-- Older rows are kept out of the DOM by the trailing
					 render window. Scrolling near the top grows the
					 window automatically (with scroll anchoring); this
					 button is the explicit affordance + a count so the
					 user knows there's history above and isn't confused
					 by a transcript that starts mid-conversation. -->
				<button type="button" class="load-older" onclick={loadOlderRows}>
					Load {Math.min(WINDOW_GROW_STEP, hiddenAbove)} older
					{hiddenAbove === 1 ? 'message' : 'messages'} ({hiddenAbove} hidden)
				</button>
			{/if}
			{#each windowedRows as row (row.id)}
				{@render rowMarkup(row, true)}
			{/each}
			{#if hiddenBelow > 0}
				<!-- Newer rows clipped off the bottom by a detached
					 (slid) window. In-flow pill at the end of the
					 rendered rows: scrolling down toward it reels newer
					 rows in a chunk at a time (with scroll anchoring),
					 symmetric with "Load older" at the top. The floating
					 "Jump to latest" button below snaps straight to the
					 tail instead. -->
				<button type="button" class="load-older load-newer" onclick={loadNewerRows}>
					Load {Math.min(WINDOW_GROW_STEP, hiddenBelow)} newer
					{hiddenBelow === 1 ? 'message' : 'messages'} ({hiddenBelow} below)
				</button>
			{/if}
			{#if hiddenBelow > 0}
				<!-- Always-reachable escape hatch back to the live tail.
					 Sticky-positioned at the bottom of the scroll
					 viewport so the user doesn't have to scroll through
					 clipped history to catch up to a streaming reply.
					 Shown only while the window's bottom edge is detached
					 from the tail (`hiddenBelow > 0`). -->
				<button type="button" class="jump-latest" onclick={jumpToLatest} title="Jump to the latest message">
					Jump to latest ↓ ({hiddenBelow} below)
				</button>
			{/if}
		</div>
		<div class="composer">
			{#if coder.attachments.length > 0}
				<!-- Attached selections / images. Selection chips
					 click through to the file at the captured
					 range; image chips show the thumbnail. Both
					 strip with the × button. -->
				<div class="attachments" role="list">
					{#each coder.attachments as attachment (attachment.id)}
						<div class="attachment" role="listitem">
							{#if attachment.kind === 'selection'}
								<button
									type="button"
									class="attachment-open"
									title={`${attachment.path}:${attachment.startLine}-${attachment.endLine}`}
									onclick={() => onOpenAttachment(attachment)}
								>
									<FileIcon />
									<span class="attachment-label">
										<span class="attachment-name">{baseName(attachment.path)}</span>
										<span class="attachment-range">
											{attachment.startLine === attachment.endLine
												? `:${attachment.startLine}`
												: `:${attachment.startLine}-${attachment.endLine}`}
										</span>
									</span>
								</button>
							{:else if attachment.kind === 'image'}
								<span class="attachment-open attachment-image" title={attachment.name}>
									<img src={attachment.dataUrl} alt={attachment.name} class="attachment-thumb" />
									<span class="attachment-label">
										<span class="attachment-name">{attachment.name}</span>
									</span>
								</span>
							{:else}
								<span
									class="attachment-open attachment-terminal"
									title={`Terminal output (${attachment.lineCount} ${attachment.lineCount === 1 ? 'line' : 'lines'} from ${attachment.label})`}
								>
									<span class="attachment-terminal-glyph" aria-hidden="true">⌘</span>
									<span class="attachment-label">
										<span class="attachment-name">{attachment.label || 'terminal'}</span>
										<span class="attachment-range">
											:{attachment.lineCount}{attachment.lineCount === 1 ? ' line' : ' lines'}
										</span>
									</span>
								</span>
							{/if}
							<button
								type="button"
								class="attachment-remove"
								title="Remove attachment"
								aria-label="Remove attachment"
								onclick={() => coder.removeAttachment(attachment.id)}
							>
								×
							</button>
						</div>
					{/each}
				</div>
			{/if}
			{#if mention !== null}
				<!-- `@`-mention picker. Mounts on `@`, dismisses on
				     space / Esc / blur / no-match. Mouse picks fire
				     on `mousedown` (not click) so the blur that
				     follows doesn't dismiss the menu before the pick
				     lands. -->
				<div class="mention-menu" role="listbox" aria-label="File suggestions">
					{#if mention.query.length === 0}
						<div class="mention-hint">Type to filter files…</div>
					{:else if mentionResults.length === 0}
						<div class="mention-hint">
							{mentionLoading ? 'Searching…' : 'No matches'}
						</div>
					{:else}
						{#each mentionResults as hit, i (hit.path)}
							<button
								type="button"
								class="mention-row"
								class:active={i === mentionSelected}
								role="option"
								aria-selected={i === mentionSelected}
								title={hit.path}
								onmousedown={(e) => {
									e.preventDefault();
									pickMention(i);
								}}
								onmouseenter={() => (mentionSelected = i)}
							>
								<span class="mention-name">{baseName(hit.path)}</span>
								<span class="mention-path">{hit.path}</span>
							</button>
						{/each}
					{/if}
				</div>
			{/if}
			<textarea
				bind:this={composer}
				use:textInputUndo
				placeholder={coder.busy
					? 'Steer the running turn (Enter to send, Esc to stop)…'
					: coder.attachments.length > 0
						? 'Ask about the attached selection…'
						: 'Ask the coder… (@ to attach a file, paste images)'}
				rows="3"
				onkeydown={onComposerKey}
				oninput={onComposerInput}
				onpaste={onComposerPaste}
				onblur={onComposerBlur}
			></textarea>
		</div>
	{:else if coder.view === 'subagent'}
		<!-- Sub-agent pop-out: full transcript of one sub-agent, with
			 a back-arrow returning to the parent's session. No
			 composer (sub-agents take their task at spawn time and
			 finish on their own; the user can't drive them
			 mid-flight). The row renderer is the same one the
			 parent uses, so streaming sub-agents update live in
			 this view too. -->
		{@const subId = coder.viewSubagentId}
		{@const transcript = subId !== null ? (coder.subagentTranscripts.get(subId) ?? null) : null}
		<header class="session-bar">
			<button
				type="button"
				class="icon"
				onclick={() => coder.closeSubagentView()}
				title="Back to parent"
				aria-label="Back to parent"
			>
				← Back
			</button>
			{#if transcript !== null}
				<span class="session-bar-title" title={transcript.targetFolder}>
					Sub-agent · {baseName(transcript.targetFolder)}
				</span>
				<span class="subagent-mode" class:research={transcript.mode === 'research'} title="Sub-agent mode">
					{transcript.mode}
				</span>
				{#if subId !== null}
					<button
						type="button"
						class="icon"
						onclick={() => onOpenTrace(null, subId)}
						title="Open trace in editor"
						aria-label="Open trace in editor"
					>
						<CodeIcon />
					</button>
				{/if}
			{:else}
				<span class="session-bar-title">Sub-agent</span>
				<span></span>
			{/if}
		</header>
		<div class="transcript">
			{#if transcript === null}
				<p class="hint">Sub-agent transcript not available. Re-open the parent session to refresh.</p>
			{:else if transcript.rows.length === 0}
				<p class="hint">Sub-agent starting…</p>
			{:else}
				{#each transcript.rows as row (row.id)}
					{@render rowMarkup(row, false)}
				{/each}
			{/if}
		</div>
	{/if}
</div>

{#if coder.deviceCode || coder.awaitingApproval}
	<CoderConnectModal />
{/if}

{#if modelSettingsOpen}
	<CoderModelSettingsModal onClose={() => (modelSettingsOpen = false)} />
{/if}

{#if hubSettingsOpen}
	<HfBucketSettingsModal onClose={() => (hubSettingsOpen = false)} />
{/if}

{#if lightboxUrl}
	<!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions -->
	<div
		class="lightbox-backdrop"
		onclick={() => (lightboxUrl = null)}
		role="dialog"
		aria-modal="true"
		aria-label="Image preview"
		tabindex="-1"
	>
		<img class="lightbox-image" src={lightboxUrl} alt="Pasted attachment at full size" />
		<button type="button" class="lightbox-close" title="Close (Esc)" onclick={() => (lightboxUrl = null)}>×</button>
	</div>
{/if}

<!-- Row renderer extracted as a snippet so the parent's session
	 transcript and the sub-agent pop-out can share it without
	 duplicating ~80 lines of conditional markup. `withSubagentCards`
	 controls whether `task` tool rows render the inline collapsed
	 card; sub-agents themselves can't spawn sub-sub-agents (depth-1
	 cap), so the flag is `false` in the sub-agent view. -->
{#snippet compactionMarkup(state: Extract<CoderRow, { kind: 'compaction' }>)}
	<!-- Compaction disclosure rendered inline at the point the
		 fold happened, so it scrolls away under later turns
		 instead of staying pinned to the bottom. While the
		 fast-model summary call is in flight the row shows a
		 "compacting…" pip; on completion it flips to a `<details>`
		 with the synthetic summary that the agent now sees in
		 place of the older middle of the history. -->
	<div class="row compaction" class:running={state.phase === 'running'}>
		<div class="row-label">compaction</div>
		{#if state.phase === 'running'}
			<div class="bubble">
				Compacting older turns into a summary
				{#if state.messagesCompacted > 0}
					({state.messagesCompacted} messages)
				{/if}
				…
			</div>
		{:else}
			<details class="compaction-details">
				<summary>
					Compacted {state.messagesCompacted} earlier message{state.messagesCompacted === 1 ? '' : 's'} into a summary
				</summary>
				<CoderMarkdown text={state.summary} />
			</details>
		{/if}
	</div>
{/snippet}

{#snippet rowMarkup(row: CoderRow, withSubagentCards: boolean)}
	{#if row.kind === 'user'}
		{@const parsed = parseUserPrompt(row.text)}
		<div class="row user" class:queued={row.queued}>
			<div class="row-label">
				you{#if row.queued}<span
						class="queued-tag"
						title="Waiting for the current turn to finish. Press ↑ on an empty composer to pull it back.">queued</span
					>{/if}
				{#if !row.queued && !coder.busy}
					<!-- Hover-revealed revert affordances. Hidden while a
							 turn runs (the backend refuses mid-turn) and on
							 queued steers (not yet on disk, so no ordinal). -->
					<span class="row-actions">
						<button
							type="button"
							class="row-action"
							title="Edit & resend — load this message back into the composer and drop everything after it"
							onclick={() => onEditAndResend(row.id)}
						>
							<EditIcon size={12} />
						</button>
						<button
							type="button"
							class="row-action"
							title="Revert to here — permanently remove this message and everything after it"
							onclick={() => onRevertToMessage(row.id)}
						>
							<RevertIcon size={12} />
						</button>
					</span>
				{/if}
			</div>
			{#if parsed.prose.trim().length > 0}
				<div class="bubble">{parsed.prose}</div>
			{/if}
			{#if row.images.length > 0}
				<!-- Pasted images, rendered as thumbnails so the
								 user can recognise what they attached two
								 turns ago. Clicking opens an in-app
								 lightbox at full size. -->
				<div class="user-images">
					{#each row.images as img, i (img.data_url + ':' + i)}
						<button
							type="button"
							class="user-image"
							title="Open image full-size"
							onclick={() => (lightboxUrl = img.data_url)}
						>
							<img src={img.data_url} alt={`pasted image ${i + 1}`} />
						</button>
					{/each}
				</div>
			{/if}
			{#if parsed.attachments.length > 0}
				<!-- The context block the user attached at send
								 time, rendered as clickable chips instead
								 of a verbatim XML wall in the bubble.
								 Clicking opens the file at the captured
								 starting line — the file may have changed
								 since (the agent likely just edited it),
								 so this is a "navigate to the spot I
								 referenced" gesture, not "show me what I
								 sent". -->
				<div class="user-refs">
					{#each parsed.attachments as ref, i (ref.path + ':' + ref.startLine + '-' + ref.endLine + ':' + i)}
						<button
							type="button"
							class="user-ref"
							title={`${ref.path}:${ref.startLine}-${ref.endLine}`}
							onclick={() =>
								onOpenAttachment({
									path: ref.path,
									startLine: ref.startLine,
								})}
						>
							<FileIcon />
							<span class="user-ref-label">
								<span class="user-ref-name">{baseName(ref.path)}</span>
								<span class="user-ref-range"
									>{ref.startLine === ref.endLine ? `:${ref.startLine}` : `:${ref.startLine}-${ref.endLine}`}</span
								>
							</span>
						</button>
					{/each}
				</div>
			{/if}
		</div>
	{:else if row.kind === 'assistant'}
		<!-- Skip the whole row when an assistant turn produced
		     neither thinking nor text. Tool-only turns (the model
		     emits a tool call and nothing else) used to render an
		     orphan "coder" label above the tool row, which read as
		     duplicate noise. See the `kind === 'tool'` branch
		     below for the affordance the user actually cares
		     about in that case. -->
		{@const hasThinking = row.thinking.length > 0}
		{@const hasText = row.text.trim().length > 0}
		{#if hasThinking || hasText}
			<div class="row assistant">
				<div class="row-label">coder</div>
				{#if hasThinking}
					<!-- Reasoning trace. Open while streaming so the user
									 sees thoughts land, collapsed once the message
									 finishes (the `assistant_message_end` handler
									 flips `thinkingOpen`). The component pins the
									 inner scroll to the bottom only while pinned by
									 the user, same gesture as a chat thread. -->
					<CoderThinking
						text={row.thinking}
						open={row.thinkingOpen}
						onOpenChange={(next) => (row.thinkingOpen = next)}
						streaming={!hasText}
					/>
				{/if}
				{#if hasText}
					<!-- Trim before the visibility check so a
									 model that ends with just whitespace
									 (e.g. tool-only turn that emitted a
									 trailing `\n`) doesn't render an
									 empty grey rectangle below the
									 thinking block. The actual text we
									 hand to `CoderMarkdown` is untrimmed
									 — preserving leading whitespace is
									 the renderer's job. -->
					<div class="bubble assistant-bubble">
						<CoderMarkdown text={row.text} />
						<!-- Hover-revealed "Copy markdown" button. Sits
									 in the top-right corner of the bubble;
									 grabs the raw markdown source (`row.text`)
									 rather than the rendered HTML so the user
									 ends up with something they can paste back
									 into a markdown surface. The fenced-code
									 "Copy" buttons (rendered by `markdown.ts`)
									 are still active inside the bubble for
									 per-snippet copies. -->
						<button
							type="button"
							class="copy-md"
							aria-label="Copy markdown"
							onclick={(event) => {
								void onCopyAssistantMarkdown(event, row.text);
							}}
						>
							Copy markdown
						</button>
					</div>
				{/if}
			</div>
		{/if}
	{:else if row.kind === 'tool' && row.name === 'ask_user'}
		<!-- `ask_user` is interactive and must stay visible + actionable
		     while the call is in flight — so it renders an always-open
		     card rather than the collapsed `<details>` every other tool
		     row uses (whose body is lazy-mounted only on first expand).
		     The card itself flips to a read-only summary once the
		     `tool_result` lands (answered or skipped). -->
		<div class="row tool ask-user" class:err={row.isError}>
			<div class="ask-user-header">
				<span class="tool-dot" class:running={!row.hasResult} class:err={row.isError} aria-hidden="true"></span>
				<span class="tool-name">ask_user</span>
				<span class="tool-status">{!row.hasResult ? 'waiting for you…' : row.isError ? 'error' : 'done'}</span>
			</div>
			<ToolBodyAskUser args={row.args} result={row.result} hasResult={row.hasResult} callId={row.id} />
		</div>
	{:else if row.kind === 'tool'}
		{@const subagent = withSubagentCards ? (coder.subagentSummaries.get(row.id) ?? null) : null}
		{@const elapsedMs = row.hasResult ? (row.durationMs ?? 0) : Math.max(0, nowTick - row.startedAt)}
		{@const hint = toolHint(row.name, row.args)}
		<div class="row tool" class:err={row.isError}>
			<!-- One-line collapsed shape: status dot, tool name,
				 status word, elapsed counter — chevron on the right
				 from the native `<details>`. The standalone
				 `tool · {name}` label that used to sit above the
				 details was carrying duplicate information; folding
				 the name into the summary trades two short lines
				 plus the inter-line gap for one. The args / result
				 blocks render unchanged when the row is expanded. -->
			<details ontoggle={(event) => onToolToggle(event, row.id)}>
				<summary>
					<span class="tool-dot" class:running={!row.hasResult} class:err={row.isError} aria-hidden="true"></span>
					<span class="tool-name">{row.name}</span>
					{#if hint !== null}
						<!-- Identifying argument shown inline so the user
							 can recognise the call without expanding the
							 row: path for file tools, command for bash,
							 pattern / query / URL for the rest. Flexes to
							 fill the remaining width and ellipses on
							 overflow; the expanded body still has the
							 full payload. -->
						<span class="tool-hint" title={hint}>{hint}</span>
					{/if}
					<span class="tool-status">{!row.hasResult ? 'running…' : row.isError ? 'error' : 'ok'}</span>
					<!-- Live elapsed counter while running, precise
						 final duration once the tool settles. Reads
						 the panel-level `nowTick` so every running
						 tool row advances on the same 250ms beat
						 instead of each one fighting for its own
						 interval. -->
					<span class="tool-elapsed" class:running={!row.hasResult}>{fmtElapsed(elapsedMs, !row.hasResult)}</span>
				</summary>
				<!-- Body renders lazily: only once the row's `<details>`
					 has been opened (`openedToolRows.has(row.id)`). A
					 collapsed `<details>` keeps slotted children mounted,
					 so without this gate every `ToolBody*` would run its
					 grammar-load + highlight pass on first paint even
					 though the row is closed. Deferring to first-open
					 keeps the initial transcript render cheap regardless
					 of how many tool calls the session holds. -->
				{#if openedToolRows.has(row.id)}
					{#if row.name === 'bash'}
						{@const bArgs = parseBashArgs(row.args)}
						{@const bResult = row.hasResult ? parseBashResult(row.result) : null}
						{@const bashCmd = bResult?.cmd ?? bArgs?.cmd ?? ''}
						<!-- Terminal-style view: a `$ <cmd>` line, then
						 stdout / stderr blocks, then an `exit N` tag.
						 Reads like the user just ran the command in a
						 shell — much closer to the agent's mental
						 model than a JSON dump of the same fields.
						 Falls through to the JSON path below when the
						 args/result don't match the expected shape
						 (legacy traces, tool errors that returned a
						 plain string, etc.). -->
						{#if bArgs !== null || bResult !== null}
							<div class="bash-block">
								<div class="bash-cmd">
									<span class="bash-prompt" aria-hidden="true">$</span>
									<span class="bash-cmd-text">{bashCmd}</span>
								</div>
								{#if bResult !== null}
									{#if bResult.stdout.length > 0}
										<pre class="bash-stream bash-stdout">{bResult.stdout}</pre>
									{/if}
									{#if bResult.stderr.length > 0}
										<pre class="bash-stream bash-stderr">{bResult.stderr}</pre>
									{/if}
									<div class="bash-exit" class:err={bResult.exitCode !== 0 && bResult.exitCode !== null}>
										exit {bResult.exitCode ?? '?'}{#if bResult.target}
											<span class="bash-target"> · {bResult.target}</span>
										{/if}
									</div>
								{/if}
							</div>
						{:else}
							<div class="block-label">args</div>
							<pre class="block">{fmtArgs(row.args)}</pre>
							{#if row.hasResult}
								<div class="block-label">result</div>
								<pre class="block">{fmtArgs(row.result)}</pre>
							{/if}
						{/if}
					{:else if row.name === 'read_file'}
						<!-- File-viewer view: path header, line numbers
						 in a sticky column, syntax-highlighted code
						 in a scroll column. Same `@lezer/highlight`
						 pipeline that paints fenced blocks in the
						 markdown renderer, so a `.ts` snippet here
						 shares colours with the live editor. The
						 component falls back to the JSON view on
						 unrecognised payload shapes itself. -->
						<ToolBodyReadFile args={row.args} result={row.result} hasResult={row.hasResult} />
					{:else if row.name === 'write_file'}
						<!-- File-write view: header `wrote <path> · N kB`,
						 then the content rendered the same way as
						 `read_file` (line numbers + highlighting).
						 Lets the user see exactly what landed on
						 disk without an extra `read_file` round-trip. -->
						<ToolBodyWriteFile args={row.args} result={row.result} hasResult={row.hasResult} />
					{:else if row.name === 'edit_file'}
						<!-- Edit view: unified-diff style with a tinted
						 red `find` block and a tinted green `replace`
						 block. No syntax highlighting on the diff
						 sides — partial-grammar colouring of a
						 mid-expression edit is more often wrong than
						 right; the diff colours carry the signal. -->
						<ToolBodyEditFile args={row.args} result={row.result} hasResult={row.hasResult} />
					{:else if row.name === 'grep'}
						<!-- Grep view: pattern in a chip, count + truncation
						 flag in the meta line, then a scrollable hit
						 list with `path:line  text` columns. Reads
						 like `rg -n` output, which is also what the
						 model sees in its own context. -->
						<ToolBodyGrep args={row.args} result={row.result} hasResult={row.hasResult} />
					{:else if row.name === 'list_dir'}
						<!-- Listing view: kind glyph + name per row, with
						 directories accented and getting a trailing
						 `/`. A scrollable column so a large
						 `node_modules`-style listing doesn't push the
						 transcript page-tall. -->
						<ToolBodyListDir args={row.args} result={row.result} hasResult={row.hasResult} />
					{:else if row.name === 'web_search'}
						<!-- SERP view: query in a chip, result count in the
						 meta line, then one card per hit with title,
						 URL, and snippet. Clicking the title opens
						 the URL in the host's default browser via
						 `tauri-plugin-opener`. -->
						<ToolBodyWebSearch args={row.args} result={row.result} hasResult={row.hasResult} />
					{:else if row.name === 'web_fetch'}
						<!-- Page-content view: URL header (clickable to
						 open in browser) + Jina-extracted markdown
						 rendered through the same pipeline an
						 assistant message uses. Truncation flag in
						 the header when the body was lopped at the
						 200 kB cap. -->
						<ToolBodyWebFetch args={row.args} result={row.result} hasResult={row.hasResult} />
					{:else if row.name === 'todo_write'}
						<!-- Plan view: status glyph per item, in-
						 progress accented, completed / cancelled
						 struck through. The header pill renders the
						 same bucket of todos; this row shows the
						 history of plan mutations as the agent
						 works. -->
						<ToolBodyTodoWrite args={row.args} result={row.result} hasResult={row.hasResult} />
					{:else}
						<div class="block-label">args</div>
						<pre class="block">{fmtArgs(row.args)}</pre>
						{#if row.hasResult}
							<div class="block-label">result</div>
							<pre class="block">{fmtArgs(row.result)}</pre>
						{/if}
					{/if}
				{/if}
			</details>
			{#if subagent !== null}
				<!-- Collapsed sub-agent card. Renders inline under
								 the parent's `task` tool row so
								 the parent transcript stays scannable while
								 a click pops out into the full sub-agent
								 transcript view (`coder.view = 'subagent'`).
								 The mode badge inverts colour scheme by
								 mode so a research / agent mix-up is
								 obvious at a glance. -->
				<button
					type="button"
					class="subagent-card"
					class:done={subagent.status === 'done'}
					class:running={subagent.status === 'running'}
					class:err={subagent.status === 'error'}
					title={`Open sub-agent transcript (${subagent.targetFolder})`}
					onclick={() => coder.openSubagent(subagent.id)}
				>
					<div class="subagent-card-header">
						<span class="subagent-mode" class:research={subagent.mode === 'research'}>
							{subagent.mode}
						</span>
						<span class="subagent-folder" title={subagent.targetFolder}>
							{baseName(subagent.targetFolder)}
						</span>
						<span class="subagent-status">
							{#if subagent.status === 'running'}
								running…
							{:else if subagent.status === 'error'}
								error
							{:else}
								done
							{/if}
						</span>
					</div>
					{#if subagent.resultPreview && subagent.resultPreview.length > 0}
						<div class="subagent-preview">{subagent.resultPreview}</div>
					{/if}
					<div class="subagent-footer">
						{#if subagent.tokensUsedEstimate > 0}
							<span class="subagent-tokens">~{subagent.tokensUsedEstimate} tok</span>
						{:else}
							<span class="subagent-tokens placeholder">…</span>
						{/if}
						<span class="subagent-open">Open transcript →</span>
					</div>
				</button>
			{/if}
		</div>
	{:else if row.kind === 'aborted'}
		<div class="row notice">aborted</div>
	{:else if row.kind === 'error'}
		<div class="row error" role="alert">
			<div class="row-label">error</div>
			<div class="bubble">{row.text}</div>
		</div>
	{:else if row.kind === 'compaction'}
		{@render compactionMarkup(row)}
	{/if}
{/snippet}

<style>
	.panel {
		display: flex;
		flex-direction: column;
		height: 100%;
		min-height: 0;
		background: var(--m-bg-1);
		color: var(--m-fg);
	}
	.header {
		flex-shrink: 0;
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 8px;
		padding: 8px 12px;
		border-bottom: 1px solid var(--m-border);
	}
	/* Mirror `ChatPanel`'s header font: uppercase, letter-spaced,
	   11 px / muted. The coder panel layers a status dot, identity,
	   and a target chip on top of that — uniform typography keeps
	   the two right-slot tenants visually consistent without
	   stripping the extra controls coder needs. */
	.title {
		display: flex;
		align-items: center;
		gap: 6px;
		min-width: 0;
		font-weight: 600;
		text-transform: uppercase;
		letter-spacing: 0.04em;
		font-size: 11px;
		color: var(--m-fg-muted);
	}
	.label {
		color: var(--m-fg);
	}
	.who {
		text-transform: none;
		letter-spacing: 0;
		font-weight: 400;
		color: var(--m-fg-muted);
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	/* Host-vs-container indicator chip. Same monitor / container
	   glyphs the terminal tabs use (`TerminalTargetIcon`) so the
	   user reads the same visual language across the IDE. The
	   colour-mix tint on the container case keeps the boundary
	   visually obvious — running `rm -rf .` on the wrong target is
	   the kind of mistake the indicator earns its keep on. */
	.target-wrap {
		position: relative;
		display: inline-flex;
	}
	.target {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		gap: 2px;
		color: var(--m-fg-subtle);
		border: 1px solid var(--m-border);
		border-radius: 3px;
		padding: 1px 4px;
		height: 18px;
		cursor: pointer;
		background: transparent;
	}
	.target:hover {
		border-color: var(--m-border-strong, var(--m-border));
		color: var(--m-fg);
	}
	.target.container {
		color: var(--m-success);
		border-color: color-mix(in srgb, var(--m-success) 50%, transparent);
		background: color-mix(in srgb, var(--m-success) 10%, transparent);
	}
	/* Off-default badge: a session deliberately pinned to host even
	   though auto would have picked the container. */
	.target.overridden {
		color: var(--m-warning, var(--m-fg));
		border-color: color-mix(in srgb, var(--m-warning, var(--m-fg)) 55%, transparent);
		background: color-mix(in srgb, var(--m-warning, var(--m-fg)) 12%, transparent);
	}
	.override-dot {
		width: 4px;
		height: 4px;
		border-radius: 50%;
		background: currentColor;
	}
	.target-popover {
		position: absolute;
		top: calc(100% + 4px);
		left: 0;
		z-index: 30;
		width: 248px;
		display: flex;
		flex-direction: column;
		gap: 2px;
		padding: 4px;
		border: 1px solid var(--m-border);
		border-radius: 6px;
		background: var(--m-bg-elevated, var(--m-bg));
		box-shadow: 0 6px 20px rgb(0 0 0 / 0.25);
	}
	.target-popover .opt {
		display: flex;
		gap: 8px;
		align-items: flex-start;
		padding: 6px 8px;
		border: none;
		border-radius: 4px;
		background: transparent;
		text-align: left;
		cursor: pointer;
		color: var(--m-fg);
	}
	.target-popover .opt:hover {
		background: var(--m-bg-hover, color-mix(in srgb, var(--m-fg) 8%, transparent));
	}
	.target-popover .opt-radio {
		flex: none;
		margin-top: 3px;
		width: 12px;
		height: 12px;
		border-radius: 50%;
		border: 1px solid var(--m-border-strong, var(--m-border));
	}
	.target-popover .opt.selected .opt-radio {
		border-color: var(--m-accent, var(--m-fg));
		background: radial-gradient(circle, var(--m-accent, var(--m-fg)) 0 3px, transparent 4px);
	}
	.target-popover .opt-text {
		display: flex;
		flex-direction: column;
		gap: 2px;
	}
	.target-popover .opt-title {
		font-size: 12px;
		font-weight: 600;
	}
	.target-popover .opt-sub {
		font-size: 11px;
		line-height: 1.35;
		color: var(--m-fg-muted);
	}
	.target-popover .opt-sub em {
		font-style: normal;
		color: var(--m-fg-subtle);
	}
	.actions {
		display: flex;
		align-items: center;
		gap: 6px;
	}
	.stop {
		font: inherit;
		font-size: 11px;
		color: var(--m-warning, #d4a017);
		background: transparent;
		border: 1px solid var(--m-warning, #d4a017);
		border-radius: 3px;
		padding: 0 8px;
		height: 20px;
		line-height: 18px;
		cursor: pointer;
	}
	.stop:hover {
		background: color-mix(in srgb, var(--m-warning, #d4a017) 14%, transparent);
	}
	.icon {
		background: transparent;
		border: 0;
		color: var(--m-fg-muted);
		padding: 2px 4px;
		cursor: pointer;
		display: inline-flex;
		align-items: center;
	}
	.icon:hover {
		color: var(--m-fg);
	}
	.icon.active {
		color: var(--m-accent);
	}
	.icon.active:hover {
		color: var(--m-accent);
		filter: brightness(1.15);
	}
	/* Sticky in-session header with "← Sessions" + title + "+
	   new". Mirrors the chat panel's `.thread-header` shape so
	   the two right-slot tenants feel consistent. */
	.session-bar {
		flex-shrink: 0;
		display: flex;
		align-items: center;
		gap: 8px;
		padding: 6px 12px;
		border-bottom: 1px solid var(--m-border);
		background: var(--m-bg-1);
	}
	/* Both "back" and "new" sit on the strip as `.icon` buttons —
	   their styles come from the shared `.icon` rule below. */
	.session-bar-title {
		flex: 1;
		min-width: 0;
		font-size: 12px;
		font-weight: 500;
		color: var(--m-fg);
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
		text-align: center;
	}
	/* Sessions list view. Sticky header pattern matches the chat
	   panel — the section title + actions row stays glued to the
	   top while the list scrolls underneath. */
	.sessions {
		flex: 1;
		min-height: 0;
		overflow-y: auto;
		padding: 0 12px 12px;
		display: flex;
		flex-direction: column;
		gap: 8px;
	}
	.sessions-header {
		position: sticky;
		top: 0;
		z-index: 1;
		background: var(--m-bg-1);
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 8px;
		margin: 0 -12px;
		padding: 6px 12px;
		border-bottom: 1px solid var(--m-border);
	}
	.section-title {
		font-size: 11px;
		font-weight: 600;
		text-transform: uppercase;
		letter-spacing: 0.04em;
		color: var(--m-fg-muted);
	}
	.header-actions {
		display: flex;
		align-items: center;
		gap: 4px;
	}
	.session-list {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: 2px;
	}
	.session-row {
		display: flex;
		align-items: stretch;
		border-radius: 4px;
		border: 1px solid transparent;
	}
	.session-row:hover {
		background: var(--m-bg-3);
		border-color: var(--m-border);
	}
	.session-row.active {
		background: color-mix(in srgb, var(--m-accent) 12%, transparent);
		border-color: color-mix(in srgb, var(--m-accent) 50%, transparent);
	}
	.session-pick {
		flex: 1;
		min-width: 0;
		text-align: left;
		font: inherit;
		color: inherit;
		background: transparent;
		border: 0;
		cursor: pointer;
		padding: 6px 8px;
		display: flex;
		flex-direction: column;
		gap: 2px;
	}
	.session-title {
		display: flex;
		align-items: center;
		gap: 6px;
		font-size: 12px;
		color: var(--m-fg);
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.session-title-text {
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
		min-width: 0;
	}
	.session-meta {
		display: flex;
		align-items: center;
		gap: 4px;
		font-size: 11px;
		color: var(--m-fg-subtle);
	}
	.session-row.running .running-label {
		color: var(--m-accent);
		font-weight: 500;
	}
	.session-row.finished .finished-label {
		color: var(--m-warning);
		font-weight: 500;
	}
	.session-row .session-meta-sep {
		color: var(--m-fg-subtle);
	}
	/* Running pip — small accent dot that pulses while the bucket
	   reports `busy`. The session list is the only surface where a
	   user actively scans for "is anything still working?", and a
	   pulsing dot beats a static badge for that question. */
	.running-dot {
		flex-shrink: 0;
		width: 8px;
		height: 8px;
		border-radius: 50%;
		background: var(--m-accent);
		box-shadow: 0 0 0 0 color-mix(in srgb, var(--m-accent) 60%, transparent);
		animation: session-running-pulse 1.4s ease-in-out infinite;
	}
	@keyframes session-running-pulse {
		0% {
			box-shadow: 0 0 0 0 color-mix(in srgb, var(--m-accent) 60%, transparent);
		}
		70% {
			box-shadow: 0 0 0 6px color-mix(in srgb, var(--m-accent) 0%, transparent);
		}
		100% {
			box-shadow: 0 0 0 0 color-mix(in srgb, var(--m-accent) 0%, transparent);
		}
	}
	@media (prefers-reduced-motion: reduce) {
		.running-dot {
			animation: none;
		}
	}
	/* Finished marker — a static amber dot on a session whose turn
	   ended while the user was looking at a different session (or
	   the list / another folder). Mirrors the folder-bar's `.done`
	   sparkle: same `--m-warning` hue, and deliberately *no* pulse
	   — the work is done, so a static cue says "this one's waiting
	   on you" without over-claiming attention. Clears when the row
	   is opened. */
	.finished-dot {
		flex-shrink: 0;
		width: 8px;
		height: 8px;
		border-radius: 50%;
		background: var(--m-warning);
	}
	.session-row-action {
		opacity: 0;
		transition: opacity 0.1s;
	}
	.session-row:hover .session-row-action,
	.session-row:focus-within .session-row-action {
		opacity: 1;
	}
	.hub-action.synced {
		opacity: 0.55;
		color: var(--m-fg-muted);
	}
	.hub-action.synced:hover,
	.session-row:hover .hub-action.synced,
	.session-row:focus-within .hub-action.synced {
		opacity: 1;
	}
	.hub-action.syncing {
		opacity: 1;
		color: var(--m-accent);
		animation: hub-action-pulse 1.2s ease-in-out infinite;
	}
	.hub-action.failed {
		opacity: 1;
		color: var(--m-danger);
	}
	@keyframes hub-action-pulse {
		0%,
		100% {
			filter: brightness(1);
		}
		50% {
			filter: brightness(1.4);
		}
	}
	@media (prefers-reduced-motion: reduce) {
		.hub-action.syncing {
			animation: none;
		}
	}
	.empty {
		flex: 1;
		display: flex;
		flex-direction: column;
		align-items: center;
		justify-content: center;
		gap: 14px;
		padding: 24px;
		text-align: center;
	}
	.empty-lede {
		font-size: 12px;
		color: var(--m-fg-muted);
		margin: 0;
		max-width: 32ch;
		line-height: 1.5;
	}
	.primary {
		font: inherit;
		background: var(--m-accent);
		color: #fff;
		border: 0;
		border-radius: 4px;
		padding: 8px 14px;
		cursor: pointer;
	}
	.primary:hover:not(:disabled) {
		filter: brightness(1.1);
	}
	.primary:disabled {
		cursor: not-allowed;
		opacity: 0.7;
	}
	.error {
		font-size: 12px;
		color: var(--m-danger);
	}
	.transcript {
		flex: 1;
		min-height: 0;
		overflow-y: auto;
		padding: 12px;
		display: flex;
		flex-direction: column;
		gap: 12px;
	}
	.hint {
		font-size: 12px;
		color: var(--m-fg-subtle);
		margin: 0;
	}
	/* Top-of-window affordance for the trailing render window. Sits
	   above the oldest currently-mounted row when older history is
	   held out of the DOM; clicking (or scrolling near it) grows the
	   window backward. Centred, quiet, pill-shaped so it reads as a
	   control rather than a transcript row. */
	.load-older {
		align-self: center;
		flex-shrink: 0;
		font: inherit;
		font-size: 11px;
		color: var(--m-fg-muted);
		background: var(--m-bg-overlay);
		border: 1px solid var(--m-border);
		border-radius: 999px;
		padding: 3px 12px;
		cursor: pointer;
	}
	.load-older:hover {
		color: var(--m-fg);
		border-color: color-mix(in srgb, var(--m-accent) 40%, var(--m-border));
		background: color-mix(in srgb, var(--m-accent) 10%, transparent);
	}
	/* "Jump to latest" escape hatch for a detached (bottom-clipped)
	   window. Sticky to the bottom of the scroll viewport so it
	   floats over the transcript as the user scrolls, centred, with
	   a solid plate + shadow so it reads as an overlay control rather
	   than a transcript row. `margin-top: auto` keeps it from adding
	   height at the end of the flow; the negative bottom margin lets
	   it overlap the last row's gap instead of pushing layout. */
	.jump-latest {
		position: sticky;
		bottom: 8px;
		align-self: center;
		flex-shrink: 0;
		margin-top: auto;
		font: inherit;
		font-size: 11px;
		font-weight: 500;
		color: var(--m-bg);
		background: var(--m-accent);
		border: 0;
		border-radius: 999px;
		padding: 5px 14px;
		cursor: pointer;
		box-shadow: 0 4px 12px rgb(0 0 0 / 0.25);
	}
	.jump-latest:hover {
		filter: brightness(1.1);
	}
	.row {
		display: flex;
		flex-direction: column;
		gap: 4px;
	}
	.row.notice {
		font-size: 11px;
		color: var(--m-fg-subtle);
		text-align: center;
	}
	.row-label {
		display: flex;
		align-items: center;
		gap: 6px;
		font-size: 10px;
		text-transform: uppercase;
		letter-spacing: 0.06em;
		color: var(--m-fg-subtle);
	}
	.row-actions {
		display: inline-flex;
		align-items: center;
		gap: 2px;
		/* Revealed on row hover / keyboard focus so the label
			 stays uncluttered at rest. */
		opacity: 0;
		transition: opacity 0.1s ease;
	}
	.row.user:hover .row-actions,
	.row-actions:focus-within {
		opacity: 1;
	}
	.row-action {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		padding: 2px;
		border: none;
		border-radius: 4px;
		background: transparent;
		color: var(--m-fg-subtle);
		cursor: pointer;
		line-height: 0;
	}
	.row-action:hover {
		background: var(--m-bg-overlay);
		color: var(--m-fg);
	}
	.bubble {
		font-size: 13px;
		line-height: 1.5;
		white-space: pre-wrap;
		word-break: break-word;
		background: var(--m-bg-overlay);
		border-radius: 6px;
		padding: 8px 10px;
	}
	/* Assistant replies render through `CoderMarkdown`, which emits
	   real block-level HTML. `pre-wrap` on the bubble would
	   double-up by treating the model's leading `\n` characters as
	   visible blank lines on top of the markdown's already-correct
	   paragraph spacing. */
	.assistant-bubble {
		position: relative;
		white-space: normal;
	}
	/* Hover-revealed "Copy markdown" button anchored to the bubble's
	   top-right corner. Stays out of the visual flow until the user
	   actually hovers the bubble; positioned above the markdown
	   article so it doesn't displace text. The fenced-code "Copy"
	   buttons (`.md-copy-code` in `styles.css`) live inside each
	   `<pre>` block separately, so the two affordances coexist
	   without overlap when a code block sits at the very top of a
	   reply. */
	.assistant-bubble .copy-md {
		position: absolute;
		top: 6px;
		right: 6px;
		padding: 2px 8px;
		font: inherit;
		font-size: 11px;
		color: var(--m-fg-muted);
		/* Solid panel-elevation fill so the label doesn't bleed
		   through the markdown text under the button. The bubble
		   itself is `--m-bg-overlay` (a near-transparent rgba),
		   so reusing it here would render the button see-through;
		   `--m-bg-1` is the next elevation step and gives the
		   button a clear plate against the bubble. */
		background: var(--m-bg-1);
		border: 1px solid var(--m-border);
		border-radius: 3px;
		cursor: pointer;
		opacity: 0;
		transition:
			opacity 120ms ease,
			color 120ms ease,
			border-color 120ms ease;
	}
	.assistant-bubble:hover .copy-md,
	.assistant-bubble .copy-md:focus-visible {
		opacity: 1;
	}
	.assistant-bubble .copy-md:hover {
		color: var(--m-fg);
		border-color: color-mix(in srgb, var(--m-accent) 40%, var(--m-border));
	}
	.row.user .bubble {
		background: color-mix(in srgb, var(--m-accent) 18%, transparent);
	}
	/* Queued steer styling: dim the bubble + ref chips so the
	   row reads as "waiting room" instead of "live message".
	   Pairs with the `queued` tag on the row label that explains
	   the state in words for the user who isn't sure what the
	   muted colouring means. */
	.row.user.queued .bubble,
	.row.user.queued .user-ref,
	.row.user.queued .user-image {
		opacity: 0.55;
	}
	.row.user.queued .bubble {
		background: color-mix(in srgb, var(--m-accent) 8%, transparent);
		border: 1px dashed color-mix(in srgb, var(--m-accent) 40%, var(--m-border));
		padding: 7px 9px;
	}
	.queued-tag {
		margin-left: 6px;
		padding: 1px 6px;
		border-radius: 999px;
		font-size: 9px;
		letter-spacing: 0.04em;
		background: color-mix(in srgb, var(--m-accent) 22%, transparent);
		color: var(--m-fg-muted);
		text-transform: none;
		font-weight: 500;
	}
	/* Inline references attached to a user message. Sit just below
	   the prose bubble and read as quiet "links" rather than
	   primary content — the referenced code may have been edited
	   by the agent already, so these are nav affordances first,
	   citations second. */
	.user-refs {
		display: flex;
		flex-wrap: wrap;
		gap: 4px;
	}
	.user-ref {
		font: inherit;
		font-size: 11px;
		display: inline-flex;
		align-items: center;
		gap: 4px;
		padding: 2px 6px;
		background: var(--m-bg-overlay);
		color: var(--m-fg-muted);
		border: 1px solid var(--m-border);
		border-radius: 10px;
		cursor: pointer;
		max-width: 100%;
	}
	.user-ref:hover {
		color: var(--m-fg);
		background: color-mix(in srgb, var(--m-accent) 14%, transparent);
		border-color: color-mix(in srgb, var(--m-accent) 40%, var(--m-border));
	}
	.user-ref-label {
		display: inline-flex;
		align-items: baseline;
		gap: 1px;
		min-width: 0;
	}
	.user-ref-name {
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.user-ref-range {
		color: var(--m-fg-subtle);
		flex-shrink: 0;
	}
	.row.error .bubble {
		background: color-mix(in srgb, var(--m-danger) 14%, transparent);
		color: var(--m-danger);
	}
	.row.compaction .bubble {
		background: var(--m-bg-overlay);
		color: var(--m-fg-muted);
		font-style: italic;
	}
	.row.compaction.running .bubble {
		animation: compaction-pulse 1.6s ease-in-out infinite;
	}
	@keyframes compaction-pulse {
		0%,
		100% {
			opacity: 1;
		}
		50% {
			opacity: 0.55;
		}
	}
	.compaction-details {
		font-size: 12px;
		background: var(--m-bg-overlay);
		border-radius: 6px;
		padding: 6px 8px;
		color: var(--m-fg-muted);
	}
	.compaction-details summary {
		cursor: pointer;
		color: var(--m-fg-muted);
	}
	.compaction-details[open] summary {
		margin-bottom: 6px;
	}
	.row.tool details {
		font-size: 12px;
		background: var(--m-bg-overlay);
		border-radius: 6px;
		padding: 4px 8px;
	}
	/* `ask_user` renders an always-open prompt (no `<details>`,
	   no heavy box) so it flows in the transcript like an assistant
	   message rather than a collapsed tool row. Just a thin header
	   line for identity, then the free-flowing form below. */
	.row.tool.ask-user {
		font-size: 12px;
	}
	.row.tool.ask-user .ask-user-header {
		display: flex;
		align-items: center;
		gap: 6px;
		color: var(--m-fg-muted);
		min-height: 18px;
		line-height: 1.2;
	}
	.row.tool details[open] {
		padding: 6px 8px;
	}
	.row.tool.err details {
		background: color-mix(in srgb, var(--m-danger) 12%, transparent);
	}
	.row.tool summary {
		cursor: pointer;
		color: var(--m-fg-muted);
		display: flex;
		align-items: center;
		gap: 6px;
		min-height: 18px;
		line-height: 1.2;
	}
	/* Status dot: a 7px circle that flips colour with the tool's
	   state — accent while the call is in flight, danger on
	   error, otherwise a calm subtle fill. Reads as the row's
	   primary identity at a glance, so the eye can scan a
	   stack of tool rows by colour without parsing the words. */
	.row.tool .tool-dot {
		flex: 0 0 auto;
		width: 7px;
		height: 7px;
		border-radius: 50%;
		background: var(--m-fg-subtle);
	}
	.row.tool .tool-dot.running {
		background: var(--m-accent);
	}
	.row.tool .tool-dot.err,
	.row.tool.err .tool-dot {
		background: var(--m-danger);
	}
	.row.tool .tool-name {
		flex: 0 0 auto;
		color: var(--m-fg);
		font-weight: 500;
	}
	/* Identifying-argument chip between the tool name and the
	   status word. Takes the remaining width and ellipses on
	   overflow so a long path / command doesn't push the elapsed
	   counter off-row. Monospace because the values are paths,
	   shell commands, regex patterns — code-shaped text — and a
	   muted colour to keep the tool name as the primary lock-on
	   point in the row. */
	.row.tool .tool-hint {
		flex: 1 1 auto;
		min-width: 0;
		overflow: hidden;
		white-space: nowrap;
		text-overflow: ellipsis;
		font-family: var(--m-font-mono, ui-monospace, monospace);
		font-size: 11px;
		color: var(--m-fg-muted);
	}
	.row.tool .tool-status {
		flex: 0 0 auto;
	}
	/* Tabular numerics on the elapsed counter so the trailing
	   digits don't shift the layout while a running tool ticks
	   from 1.2s → 1.3s. `running` flips to a tinted muted colour
	   so a finished call's duration reads as a settled fact, not
	   an active timer. */
	.row.tool .tool-elapsed {
		flex: 0 0 auto;
		color: var(--m-fg-subtle);
		font-size: 11px;
		font-variant-numeric: tabular-nums;
	}
	.row.tool .tool-elapsed.running {
		color: var(--m-accent);
	}
	.row.tool .block-label {
		margin-top: 6px;
		font-size: 10px;
		text-transform: uppercase;
		letter-spacing: 0.06em;
		color: var(--m-fg-subtle);
	}
	.row.tool .block {
		background: var(--m-bg);
		color: var(--m-fg);
		border-radius: 4px;
		padding: 6px 8px;
		max-height: 240px;
		overflow: auto;
		font-family: var(--m-font-mono, ui-monospace, monospace);
		font-size: 11px;
		line-height: 1.4;
		margin: 4px 0 0;
		white-space: pre-wrap;
		word-break: break-word;
	}
	/* Terminal-style body for an expanded `bash` tool row. The
	   command is rendered as a `$ <cmd>` line, then stdout /
	   stderr / exit-code blocks beneath. Reads like the agent's
	   own shell session rather than a JSON object with `cmd` /
	   `stdout` / `stderr` keys, which is what you'd want to
	   debug a multi-step bash plan. */
	.row.tool .bash-block {
		display: flex;
		flex-direction: column;
		gap: 4px;
		margin-top: 4px;
		font-family: var(--m-font-mono, ui-monospace, monospace);
		font-size: 11px;
		line-height: 1.4;
	}
	.row.tool .bash-cmd {
		display: flex;
		gap: 6px;
		background: var(--m-bg);
		border-radius: 4px;
		padding: 6px 8px;
		white-space: pre-wrap;
		word-break: break-word;
	}
	.row.tool .bash-prompt {
		flex: 0 0 auto;
		color: var(--m-accent);
		user-select: none;
	}
	.row.tool .bash-cmd-text {
		flex: 1 1 auto;
		color: var(--m-fg);
	}
	.row.tool .bash-stream {
		background: var(--m-bg);
		color: var(--m-fg);
		border-radius: 4px;
		padding: 6px 8px;
		max-height: 240px;
		overflow: auto;
		margin: 0;
		white-space: pre-wrap;
		word-break: break-word;
	}
	/* stderr stays on the same dark background as stdout so a
	   command that interleaves the two streams reads as one
	   pane; the danger tint on the text itself is enough to
	   call out the "this is the error stream" without making
	   benign stderr output (cargo build progress, ssh banners)
	   look like a fatal failure. */
	.row.tool .bash-stderr {
		color: var(--m-danger);
	}
	.row.tool .bash-exit {
		font-size: 10px;
		text-transform: uppercase;
		letter-spacing: 0.06em;
		color: var(--m-fg-subtle);
	}
	.row.tool .bash-exit.err {
		color: var(--m-danger);
	}
	.row.tool .bash-target {
		color: var(--m-fg-subtle);
		text-transform: none;
		letter-spacing: 0;
	}
	/* Collapsed sub-agent card. Renders inline under the parent's
	   `task` tool row. Reads as a clickable "pop out"
	   affordance — click anywhere on the card body, and the panel
	   swaps to the sub-agent's own transcript view. */
	.subagent-card {
		appearance: none;
		display: flex;
		flex-direction: column;
		gap: 4px;
		margin-top: 6px;
		padding: 8px 10px;
		background: var(--m-bg-overlay);
		border: 1px solid var(--m-border);
		border-radius: 6px;
		color: var(--m-fg);
		text-align: left;
		font: inherit;
		font-size: 12px;
		cursor: pointer;
	}
	.subagent-card:hover {
		background: var(--m-bg-2);
		border-color: var(--m-border-strong, var(--m-border));
	}
	.subagent-card.running {
		border-style: dashed;
	}
	.subagent-card.err {
		border-color: var(--m-danger);
		background: color-mix(in srgb, var(--m-danger) 10%, transparent);
	}
	.subagent-card-header {
		display: flex;
		align-items: center;
		gap: 8px;
		font-size: 11px;
		color: var(--m-fg-muted);
	}
	.subagent-folder {
		flex: 1;
		min-width: 0;
		font-weight: 500;
		font-family: var(--m-font-mono, ui-monospace, monospace);
		color: var(--m-fg);
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.subagent-status {
		font-size: 10px;
		text-transform: uppercase;
		letter-spacing: 0.06em;
	}
	/* Mode pill: `agent` keeps the accent fill (full toolkit, can
	   edit), `research` flips to a quieter neutral fill so the
	   read-only constraint is visually distinct. Same shape as
	   the SCM panel's changes badge for visual continuity. */
	.subagent-mode {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		min-width: 0;
		padding: 1px 6px;
		border-radius: 999px;
		background: var(--m-accent);
		color: var(--m-bg);
		font-size: 10px;
		font-weight: 600;
		text-transform: uppercase;
		letter-spacing: 0.06em;
		flex-shrink: 0;
	}
	.subagent-mode.research {
		background: var(--m-bg-3, var(--m-bg-2));
		color: var(--m-fg-muted);
	}
	.subagent-preview {
		font-size: 12px;
		line-height: 1.4;
		color: var(--m-fg);
		display: -webkit-box;
		-webkit-line-clamp: 2;
		line-clamp: 2;
		-webkit-box-orient: vertical;
		overflow: hidden;
	}
	.subagent-footer {
		display: flex;
		align-items: center;
		justify-content: space-between;
		font-size: 10px;
		color: var(--m-fg-subtle);
	}
	.subagent-open {
		font-weight: 500;
		color: var(--m-fg-muted);
	}
	/* Pre-first-round-trip placeholder — keeps the footer at a
	   stable height while the sub-agent's first `token_usage`
	   event hasn't arrived yet, without showing a stale `~0 tok`. */
	.subagent-tokens.placeholder {
		opacity: 0.5;
	}
	.composer {
		flex-shrink: 0;
		border-top: 1px solid var(--m-border);
		padding: 8px;
		display: flex;
		flex-direction: column;
		gap: 6px;
		position: relative;
	}
	.mention-menu {
		position: absolute;
		left: 8px;
		right: 8px;
		bottom: calc(100% - 8px);
		max-height: 240px;
		overflow-y: auto;
		background: var(--m-bg-2);
		border: 1px solid var(--m-border);
		border-radius: 4px;
		box-shadow: 0 4px 12px rgb(0 0 0 / 18%);
		z-index: 10;
		display: flex;
		flex-direction: column;
		padding: 4px;
		gap: 1px;
	}
	.mention-hint {
		padding: 6px 8px;
		font-size: 11px;
		color: var(--m-fg-muted);
	}
	.mention-row {
		display: flex;
		align-items: baseline;
		gap: 8px;
		padding: 4px 8px;
		background: transparent;
		border: 0;
		border-radius: 3px;
		color: var(--m-fg);
		font: inherit;
		font-size: 12px;
		cursor: pointer;
		text-align: left;
		min-width: 0;
	}
	.mention-row.active {
		background: color-mix(in srgb, var(--m-accent) 18%, transparent);
	}
	.mention-name {
		flex-shrink: 0;
		font-weight: 500;
	}
	.mention-path {
		color: var(--m-fg-muted);
		font-size: 11px;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
		min-width: 0;
	}
	.attachments {
		display: flex;
		flex-wrap: wrap;
		gap: 4px;
	}
	.attachment {
		display: inline-flex;
		align-items: stretch;
		font-size: 11px;
		background: var(--m-bg-overlay);
		border: 1px solid var(--m-border);
		border-radius: 4px;
		max-width: 100%;
		min-width: 0;
	}
	.attachment-open {
		font: inherit;
		font-size: 11px;
		display: inline-flex;
		align-items: center;
		gap: 4px;
		padding: 2px 4px 2px 6px;
		background: transparent;
		border: 0;
		color: var(--m-fg);
		cursor: pointer;
		min-width: 0;
		max-width: 220px;
	}
	.attachment-open:hover {
		background: color-mix(in srgb, var(--m-accent) 14%, transparent);
	}
	.attachment-label {
		display: inline-flex;
		align-items: baseline;
		gap: 1px;
		min-width: 0;
	}
	.attachment-name {
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.attachment-range {
		color: var(--m-fg-muted);
		flex-shrink: 0;
	}
	.attachment-remove {
		font: inherit;
		font-size: 13px;
		line-height: 1;
		color: var(--m-fg-muted);
		background: transparent;
		border: 0;
		border-left: 1px solid var(--m-border);
		padding: 0 6px;
		cursor: pointer;
	}
	.attachment-remove:hover {
		color: var(--m-fg);
		background: color-mix(in srgb, var(--m-danger) 14%, transparent);
	}
	.attachment-image {
		cursor: default;
	}
	.attachment-thumb {
		display: block;
		width: 28px;
		height: 28px;
		object-fit: cover;
		border-radius: 3px;
		flex-shrink: 0;
	}
	.attachment-terminal {
		cursor: default;
	}
	.attachment-terminal-glyph {
		font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
		color: var(--m-fg-muted);
		flex-shrink: 0;
	}
	.user-images {
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
		margin-top: 4px;
	}
	.user-image {
		display: inline-block;
		max-width: 320px;
		padding: 0;
		border: 1px solid var(--m-border);
		border-radius: 4px;
		overflow: hidden;
		background: none;
		cursor: zoom-in;
	}
	.user-image img {
		display: block;
		max-width: 100%;
		max-height: 240px;
		height: auto;
	}
	.lightbox-backdrop {
		position: fixed;
		inset: 0;
		z-index: 1000;
		display: flex;
		align-items: center;
		justify-content: center;
		padding: 32px;
		background: rgba(0, 0, 0, 0.8);
		cursor: zoom-out;
	}
	.lightbox-image {
		max-width: 100%;
		max-height: 100%;
		object-fit: contain;
		border-radius: 4px;
		box-shadow: 0 8px 32px rgba(0, 0, 0, 0.5);
	}
	.lightbox-close {
		position: absolute;
		top: 16px;
		right: 16px;
		width: 32px;
		height: 32px;
		display: flex;
		align-items: center;
		justify-content: center;
		font-size: 22px;
		line-height: 1;
		color: var(--m-fg);
		background: var(--m-bg-overlay);
		border: 1px solid var(--m-border);
		border-radius: 4px;
		cursor: pointer;
	}
	.lightbox-close:hover {
		background: var(--m-bg);
	}
	textarea {
		width: 100%;
		box-sizing: border-box;
		resize: vertical;
		min-height: 64px;
		max-height: 240px;
		font: inherit;
		font-size: 13px;
		line-height: 1.4;
		color: var(--m-fg);
		background: var(--m-bg);
		border: 1px solid var(--m-border);
		border-radius: 4px;
		padding: 8px 10px;
	}
	textarea:focus {
		outline: none;
		border-color: var(--m-accent);
	}
	textarea:disabled {
		opacity: 0.7;
	}
</style>
