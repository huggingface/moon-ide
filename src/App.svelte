<script lang="ts">
	import { onMount } from 'svelte';
	import { open } from '@tauri-apps/plugin-dialog';
	import Sidebar from './lib/components/Sidebar.svelte';
	import EditorPane from './lib/components/EditorPane.svelte';
	import StatusBar from './lib/components/StatusBar.svelte';
	import Splash from './lib/components/Splash.svelte';
	import Welcome from './lib/components/Welcome.svelte';
	import BranchSwitcher from './lib/components/BranchSwitcher.svelte';
	import CommandPalette from './lib/components/CommandPalette.svelte';
	import CompanionModal from './lib/components/CompanionModal.svelte';
	import ChatPanel from './lib/components/ChatPanel.svelte';
	import CoderPanel from './lib/components/CoderPanel.svelte';
	import BottomPanel from './lib/components/BottomPanel.svelte';
	import { workspace } from './lib/state.svelte';
	import { rightPanel } from './lib/rightPanel.svelte';
	import { coder } from './lib/coder.svelte';
	import { companion } from './lib/companion.svelte';
	import { bottomPanel } from './lib/bottomPanel.svelte';
	import { terminal } from './lib/terminal.svelte';
	import { openPreferredTerminal } from './lib/openTerminal';
	import { palette, reloadWindow, searchPaletteInitialQuery, searchQueryFromSelection } from './lib/commands.svelte';
	import { cycleFocus } from './lib/focus';
	import { ipc } from './lib/ipc';
	import { formatError } from './lib/protocol';
	import { resolveAppInfo } from './lib/workspace-id';
	import { workspacePicker } from './lib/workspacePicker.svelte';
	import { workspaceCreate } from './lib/workspaceCreate.svelte';
	import WorkspacePicker from './lib/components/WorkspacePicker.svelte';
	import WorkspaceCreate from './lib/components/WorkspaceCreate.svelte';
	import PrebootLanding from './lib/components/PrebootLanding.svelte';

	let prebootMode = $state(false);

	let sidebarWidth = $state(280);
	// The right-side slot is shared between chat and coder
	// (`rightPanel.kind`); a single width covers either tenant. The
	// coder panel ships a few extra header controls so its content
	// reads better at a slightly wider default than chat alone
	// needed, but the gap isn't worth the cost of two parallel
	// sticky widths the user has to mentally model.
	let rightPanelWidth = $state(380);
	let resizing = $state(false);
	let resizingRightPanel = $state(false);
	let resizingBottom = $state(false);

	// True when the keyboard event originated in a surface where
	// native word-motion (Alt+Arrow) is what the user actually
	// wants — the command palette search, the Slack composer, any
	// HTML form field. Used by the window-level Alt+Arrow handler
	// to opt out so CM's own keymap / the browser default still
	// runs in those cases. CM editors are NOT in this list by
	// design: Alt+Left there is our "navigate back" gesture, not
	// word-motion.
	function isTextInputTarget(target: EventTarget | null): boolean {
		if (!(target instanceof HTMLElement)) {
			return false;
		}
		if (target.isContentEditable && !target.closest('.cm-editor')) {
			return true;
		}
		const tag = target.tagName;
		return tag === 'INPUT' || tag === 'TEXTAREA';
	}

	// True when DOM focus currently sits inside an xterm.js
	// surface (the helper textarea xterm uses for input, the
	// `.xterm-screen` content layer, the `.term-host` wrapper).
	// Used by the Ctrl+L handler to prefer the terminal's
	// selection over a stale editor selection when the user is
	// actively reading scrollback. `:focus-within` would do this
	// declaratively but the JS path keeps the keymap predicate
	// alongside its fallback chain.
	function isFocusInsideTerminal(): boolean {
		const active = document.activeElement;
		if (!(active instanceof HTMLElement)) {
			return false;
		}
		return active.closest('.term-host, .xterm') !== null;
	}

	// Title bar = `<workspace-name> — <focused-folder>:<branch>`.
	// Re-synced on every change to any of the three inputs.
	// `setTitle` is per-window so each open workspace's window
	// stays labelled correctly without coordination.
	$effect(() => {
		const name = workspace.workspaceName;
		if (name === null) {
			return;
		}
		const folder = workspace.activeFolder?.name ?? null;
		const branch = workspace.gitBranch.name;
		let title = name;
		if (folder !== null) {
			title = `${name} — ${folder}`;
			if (branch !== null) {
				title = `${title}:${branch}`;
			}
		}
		void ipc.window.setTitle(title);
	});

	onMount(() => {
		void hydrate();
		const onKey = async (event: KeyboardEvent) => {
			// F6 cycle is unmodified. Shift+F6 goes the other way.
			// Treat F6 specially so it works the same way regardless of
			// whether other modifiers are held — we only branch on Shift.
			if (event.key === 'F6') {
				event.preventDefault();
				cycleFocus(!event.shiftKey);
				return;
			}
			// Alt+Left / Alt+Right = navigate back / forward through
			// file history. Intentionally global rather than an
			// editor-scoped keymap so it works from a diff tab, an
			// image tab, or the tree — anywhere a user might want
			// to step back to the previous place they were
			// reading. Always swallows the event (preventDefault +
			// stopPropagation), regardless of whether there's
			// anywhere to navigate to: a stale "Alt+Left on a fresh
			// session" shouldn't leak through to CM's word-motion
			// default and silently move the caret. Inputs /
			// textareas are exempt so word-motion still works inside
			// the command palette and similar surfaces.
			if (event.altKey && !event.ctrlKey && !event.metaKey && !event.shiftKey) {
				const arrow = event.key === 'ArrowLeft' ? 'back' : event.key === 'ArrowRight' ? 'forward' : null;
				if (arrow !== null && !isTextInputTarget(event.target)) {
					event.preventDefault();
					event.stopPropagation();
					if (arrow === 'back' && workspace.canNavigateBack) {
						void workspace.navigateBack();
					} else if (arrow === 'forward' && workspace.canNavigateForward) {
						void workspace.navigateForward();
					}
					return;
				}
				// Alt+Z: toggle soft-wrap on every editor pane. Same
				// keystroke VS Code / Cursor use, so muscle memory
				// carries over. Skipped inside text inputs / textareas
				// (palette, Slack composer, etc.) so a literal `z`
				// with the Alt modifier still types in those surfaces
				// — the editor itself doesn't have a competing native
				// behaviour for Alt+Z, but inputs do (German layout
				// dead-keys, etc.) so this is the safer default.
				if (event.key.toLowerCase() === 'z' && !isTextInputTarget(event.target)) {
					event.preventDefault();
					event.stopPropagation();
					workspace.toggleLineWrap();
					return;
				}
			}
			const ctrl = event.ctrlKey || event.metaKey;
			if (!ctrl) {
				return;
			}
			const key = event.key.toLowerCase();
			if (!event.shiftKey && key === 's') {
				event.preventDefault();
				await workspace.saveActive();
				return;
			}
			if (!event.shiftKey && key === 'w') {
				event.preventDefault();
				workspace.closeActive();
				return;
			}
			if (!event.shiftKey && key === 'n') {
				// Refuse to open an untitled tab when there is no
				// workspace: untitled buffers piggyback on the same
				// editor pane scaffolding, which only renders inside an
				// open folder. The toast tells the user what to do
				// rather than silently doing nothing.
				event.preventDefault();
				if (!workspace.workspace) {
					workspace.flash('Open a folder before creating a new file.');
					return;
				}
				workspace.newUntitledTab();
				return;
			}
			if (!event.shiftKey && key === 'o') {
				// Open File…: native picker, then route through
				// `openHostFile`. Files inside the active folder
				// fall through to the regular `openFile` flow;
				// files outside it (or, in the Phase 2 container
				// world, outside the bind mount) are read via
				// `fs.readFileHost` and tracked with `isExternal`.
				// Same toast-on-no-folder shape as Ctrl+N because
				// the open-files list is per-folder.
				event.preventDefault();
				if (!workspace.workspace) {
					workspace.flash('Open a folder before opening a file.');
					return;
				}
				const selected = await open({ directory: false, multiple: false });
				if (typeof selected === 'string') {
					await workspace.openHostFile(selected);
				}
				return;
			}
			if (event.shiftKey && key === 'p') {
				event.preventDefault();
				palette.show('commands');
				return;
			}
			if (!event.shiftKey && key === 'p') {
				event.preventDefault();
				palette.show('files');
				return;
			}
			if (event.shiftKey && key === 'f') {
				event.preventDefault();
				// Pre-fill with the editor selection, mirroring
				// VS Code / Cursor — and when there's nothing
				// selected, fall back to the last needle we ran
				// this session so reopening the palette doesn't
				// force a retype. The input auto-selects on focus
				// so the user can immediately retype to replace,
				// or just hit Enter to re-run the search. Force
				// the replace row closed so the muscle-memory
				// "open search" shortcut never lands on the wider
				// refactor layout if a previous Ctrl+Shift+H left
				// it open.
				palette.setReplaceOpen(false);
				palette.show('search', searchPaletteInitialQuery());
				return;
			}
			if (event.shiftKey && key === 'h') {
				// Mass replace across files. Same selection-prefill
				// as Ctrl+Shift+F; the only difference is the
				// replace row opens automatically and focuses the
				// replacement input when the query is already
				// populated (see CommandPalette.svelte's focus
				// effect). Matches VS Code / IntelliJ's bindings so
				// users coming from those tools don't have to
				// re-learn it.
				event.preventDefault();
				palette.setReplaceOpen(true);
				palette.show('search', searchQueryFromSelection());
				return;
			}
			if (event.shiftKey && key === 'b') {
				// Branch switcher: recent local branches + open
				// GitHub PRs in one palette. Requires a workspace
				// — the `git for-each-ref` runs against the active
				// folder. Press-with-no-folder flashes a hint
				// instead of opening an empty palette so the user
				// gets the explanation, not silence.
				event.preventDefault();
				if (!workspace.workspace) {
					workspace.flash('Open a folder before switching branches.');
					return;
				}
				workspace.openBranchSwitcher();
				return;
			}
			if (event.shiftKey && key === 'd') {
				// Git: Toggle Diff View. Hidden by the command's
				// own visibility check when there's nothing to
				// diff (clean / untracked / added / deleted /
				// untitled / non-text), so press-and-no-op is the
				// honest fallback when the user fires it on a
				// buffer that doesn't qualify. Always swallow so
				// the press doesn't leak through to anything else.
				event.preventDefault();
				const path = workspace.activePath;
				if (path === null) {
					return;
				}
				const file = workspace.openFiles.find((f) => f.path === path);
				if (!file || file.kind !== 'text' || file.isDeleted || file.isUntitled) {
					return;
				}
				const status = workspace.gitStatusEntries.find((e) => e.path === path)?.status;
				if (status !== 'modified') {
					return;
				}
				workspace.toggleDiffMode(path);
				return;
			}
			if (key === '\\') {
				event.preventDefault();
				if (workspace.hasSplit) {
					workspace.closeSplit();
				} else {
					workspace.splitActive('right');
				}
				return;
			}
			if (!event.shiftKey && key === 'r') {
				event.preventDefault();
				await reloadWindow();
				return;
			}
			// Phase 7.8 — multi-workspace shortcuts.
			//
			// Ctrl+Shift+N: open the "New workspace" modal in
			// this window. Submitting creates the workspace and
			// spawns a window for it; the calling window stays
			// where it was so the user keeps their context if
			// they bail.
			if (event.shiftKey && key === 'n') {
				event.preventDefault();
				workspaceCreate.open();
				return;
			}
			// Ctrl+Shift+O: workspace picker palette. Selecting
			// an entry focuses an existing window for that
			// workspace or spawns a fresh one.
			if (event.shiftKey && key === 'o') {
				event.preventDefault();
				void workspacePicker.open();
				return;
			}
			// Ctrl+Shift+A: add a folder to this window's workspace.
			// Same picker as the welcome screen's and folder bar's
			// `Add folder` button.
			if (event.shiftKey && key === 'a') {
				event.preventDefault();
				void pickFolder();
				return;
			}
			// Ctrl+Shift+W: close the calling window. The
			// last-window guard lives on the backend (refuses
			// to close it); the toast feedback path here keeps
			// that explicit for the user.
			if (event.shiftKey && key === 'w') {
				event.preventDefault();
				try {
					await ipc.window.close();
				} catch (err) {
					workspace.flash(formatError(err));
				}
				return;
			}
			if (!event.shiftKey && key === 'l') {
				// Echoes Cursor's `Ctrl+L = open coder chat` muscle
				// memory:
				//   - if the *focused* surface is a terminal pane
				//     and its selection is non-empty, attach the
				//     highlighted scrollback as a `<terminal_output>`
				//     chip — focus beats a stale editor selection
				//     left over from the user's previous task;
				//   - else if the editor has a non-empty selection,
				//     attach it to the coder composer as a chip;
				//   - else if any terminal pane has a non-empty
				//     selection, fall back to that;
				//   - otherwise just toggle coder visibility.
				// Slack still has its status-bar pip + the
				// speech-bubble swap icon in the coder header + the
				// `chat.togglePanel` palette entry, so giving up its
				// own Ctrl+L doesn't hide it from anybody.
				event.preventDefault();
				const editorSelection = workspace.activeSelection;
				const terminalSelection = terminal.activeSelection;
				const inTerminal = isFocusInsideTerminal();
				if (inTerminal && terminalSelection !== null) {
					coder.addAttachmentFromTerminal({
						text: terminalSelection.text,
						label: terminalSelection.label,
					});
					return;
				}
				if (editorSelection !== null) {
					coder.addAttachmentFromSelection(editorSelection);
					return;
				}
				if (terminalSelection !== null) {
					coder.addAttachmentFromTerminal({
						text: terminalSelection.text,
						label: terminalSelection.label,
					});
					return;
				}
				coder.togglePanel();
				return;
			}
			if (!event.shiftKey && key === 'j') {
				// VSCode-style "show panel" toggle. Picks up service
				// log streams (and eventually terminals) — both live
				// in the bottom panel. We swallow the event regardless
				// of focus so the user can hit it from anywhere.
				event.preventDefault();
				const wasVisible = bottomPanel.visible;
				bottomPanel.toggle();
				// Auto-spawn a terminal when the user opens an empty
				// panel — same default the launch-time
				// `spawnInitialBottomPanelTerminal` applies, just on
				// the keystroke instead of at startup. Skip when the
				// panel was already visible (we're hiding it) or
				// when there's already at least one tab to focus.
				if (!wasVisible && bottomPanel.tabs.length === 0 && workspace.workspace) {
					void openPreferredTerminal();
				}
				return;
			}
			// Don't filter by Shift: French AZERTY needs Shift to type
			// a literal `0` (the digit row produces accented letters
			// otherwise), so the natural binding there is Ctrl+Shift+0.
			// On QWERTY it's plain Ctrl+0. Matching on the typed
			// character (`key`) lets both presses fire the same
			// shortcut without us caring about layout.
			if (key === '0') {
				event.preventDefault();
				workspace.requestSidebarFocus();
				return;
			}
		};
		// Capture-phase listener. Pierre's `@pierre/trees` web
		// component (and occasionally other focusable surfaces)
		// swallow ArrowLeft / ArrowRight for their own internal
		// navigation via `stopPropagation()`. Capturing at the
		// window means our `Alt+Arrow` handler runs *before* any
		// descendant component sees the event, so navigation works
		// from the tree and anywhere else focus might have
		// wandered. The rest of the shortcuts in this listener are
		// Ctrl/Cmd-based and wouldn't collide with descendant
		// handlers either way; capture is strictly a fix for the
		// Alt+Arrow case but doesn't harm the others.
		window.addEventListener('keydown', onKey, true);
		return () => window.removeEventListener('keydown', onKey, true);
	});

	async function hydrate() {
		// Resolve the process's mode + workspace before any
		// other IPC fires. The answer never changes for the
		// process's lifetime; subsequent `currentWorkspaceId()`
		// reads return the cached value.
		const info = await resolveAppInfo();
		if (info.mode === 'preboot') {
			// Empty-catalog first launch. Render the
			// "Name your workspace" landing instead of the
			// regular IDE chrome; submitting spawns a real
			// `--workspace <slug>` child and exits this
			// process.
			prebootMode = true;
			workspace.hydrated = true;
			return;
		}
		workspace.workspaceName = info.workspaceName ?? info.workspaceId;
		// The backend has already replayed the persisted folder list and
		// active-folder pointer at launch (see src-tauri/src/lib.rs),
		// so the first call to `workspace_active` returns the full,
		// correct shape. We then let `restoreAppState` fill in the
		// per-folder UI state (open tabs etc.) from `app_state.json`.
		try {
			const ws = await ipc.workspace.active();
			if (ws) {
				await workspace.adoptWorkspaceSnapshot(ws);
			}
			// Always restore app state — theme applies even with no workspace
			// (the welcome screen still respects the saved theme).
			await workspace.restoreAppState();
		} finally {
			// Belt-and-braces: `restoreAppState` already flips the flag
			// on every exit it controls, but if anything upstream throws
			// we still need to leave the splash. Idempotent.
			workspace.hydrated = true;
		}
	}

	async function pickFolder() {
		const selected = await open({ directory: true, multiple: false });
		if (typeof selected !== 'string') {
			return;
		}
		await workspace.openLocal(selected);
	}

	function startResize(event: PointerEvent) {
		resizing = true;
		const startX = event.clientX;
		const startW = sidebarWidth;

		const onMove = (e: PointerEvent) => {
			const next = startW + (e.clientX - startX);
			sidebarWidth = Math.max(180, Math.min(500, next));
		};
		const onUp = () => {
			resizing = false;
			window.removeEventListener('pointermove', onMove);
			window.removeEventListener('pointerup', onUp);
		};
		window.addEventListener('pointermove', onMove);
		window.addEventListener('pointerup', onUp);
	}

	function startBottomResize(event: PointerEvent) {
		// Vertical resize: dragging the handle up grows the panel.
		// Subtract the delta because the panel is anchored to the
		// bottom of `.main`. Clamp lives in `bottomPanel.setHeight`.
		resizingBottom = true;
		const startY = event.clientY;
		const startH = bottomPanel.height;

		const onMove = (e: PointerEvent) => {
			bottomPanel.setHeight(startH - (e.clientY - startY));
		};
		const onUp = () => {
			resizingBottom = false;
			window.removeEventListener('pointermove', onMove);
			window.removeEventListener('pointerup', onUp);
		};
		window.addEventListener('pointermove', onMove);
		window.addEventListener('pointerup', onUp);
	}

	function startRightPanelResize(event: PointerEvent) {
		// Drag direction is mirrored vs. the sidebar handle: dragging
		// the handle left grows the panel (it sits on the right edge
		// of the editor area). One width covers both tenants of the
		// right-side slot — see `rightPanelWidth`.
		resizingRightPanel = true;
		const startX = event.clientX;
		const startW = rightPanelWidth;

		const onMove = (e: PointerEvent) => {
			const next = startW - (e.clientX - startX);
			rightPanelWidth = Math.max(240, Math.min(720, next));
		};
		const onUp = () => {
			resizingRightPanel = false;
			window.removeEventListener('pointermove', onMove);
			window.removeEventListener('pointerup', onUp);
		};
		window.addEventListener('pointermove', onMove);
		window.addEventListener('pointerup', onUp);
	}
</script>

{#if !workspace.hydrated}
	<!-- Holds the viewport until we know whether there's a workspace
	     and which theme to paint in. Otherwise the Welcome screen
	     flashes its `Add folder` card under dark themes on every
	     launch of a project that was already open. -->
	<Splash />
{:else if prebootMode}
	<!-- First launch with an empty catalog. The user names a
	     workspace; we spawn a real `--workspace <slug>` child
	     and exit this preboot process. -->
	<PrebootLanding />
{:else}
	<div class="app">
		<aside class="sidebar" style:width="{sidebarWidth}px">
			<Sidebar onPickFolder={pickFolder} />
		</aside>
		<div
			class="resize"
			class:active={resizing}
			role="separator"
			aria-orientation="vertical"
			aria-label="Resize sidebar"
			tabindex="-1"
			onpointerdown={startResize}
		></div>
		<main class="main">
			{#if workspace.workspace}
				<div class="editor-area">
					<EditorPane side="left" />
					{#if workspace.hasSplit}
						<div class="pane-divider"></div>
						<EditorPane side="right" />
					{/if}
				</div>
			{:else}
				<Welcome onPickFolder={pickFolder} />
			{/if}
			{#if bottomPanel.visible}
				<div
					class="resize bottom-resize"
					class:active={resizingBottom}
					role="separator"
					aria-orientation="horizontal"
					aria-label="Resize bottom panel"
					tabindex="-1"
					onpointerdown={startBottomResize}
				></div>
				<div class="bottom-host" style:height="{bottomPanel.height}px">
					<BottomPanel />
				</div>
			{/if}
		</main>
		<!-- Single right-side slot. Chat and coder are mutually
			 exclusive tenants of it (`rightPanel.kind`); both share
			 the same width so toggling between them doesn't reflow
			 the editor area. -->
		{#if rightPanel.kind !== null}
			<div
				class="resize chat-resize"
				class:active={resizingRightPanel}
				role="separator"
				aria-orientation="vertical"
				aria-label="Resize side panel"
				tabindex="-1"
				onpointerdown={startRightPanelResize}
			></div>
			<aside class="right-panel" style:width="{rightPanelWidth}px">
				{#if rightPanel.kind === 'chat'}
					<ChatPanel />
				{:else if rightPanel.kind === 'coder'}
					<CoderPanel />
				{/if}
			</aside>
		{/if}
	</div>
	<StatusBar />
	<CommandPalette />
	<BranchSwitcher />
	<WorkspacePicker />
	<WorkspaceCreate />
	{#if companion.modalOpen}
		<CompanionModal />
	{/if}
	{#if workspace.toast}
		<div class="toast" role="status">{workspace.toast}</div>
	{/if}
{/if}

<style>
	.app {
		display: flex;
		height: calc(100vh - 24px);
		overflow: hidden;
	}

	.sidebar {
		flex-shrink: 0;
		background: var(--m-bg-1);
		border-right: 1px solid var(--m-border);
		overflow: hidden;
		display: flex;
		flex-direction: column;
	}

	.resize {
		width: 4px;
		margin-left: -2px;
		cursor: col-resize;
		background: transparent;
		z-index: 5;
	}
	.chat-resize {
		/* Splitter sits between the editor area and the chat panel.
		   Bleed into both sides so the hit target is wide enough to
		   grab without picking the editor scrollbar by accident. */
		margin-left: -2px;
		margin-right: -2px;
	}
	/* Horizontal splitter between the editor area and the bottom
	   panel. Same hit-target bleed trick as the vertical handles
	   so it's grabbable without snagging the tab strip. */
	.bottom-resize {
		width: auto;
		height: 4px;
		margin: -2px 0;
		cursor: row-resize;
	}
	.resize:hover,
	.resize.active {
		background: var(--m-accent);
	}

	.main {
		flex: 1;
		min-width: 0;
		display: flex;
		flex-direction: column;
		background: var(--m-bg);
	}

	.right-panel {
		flex-shrink: 0;
		background: var(--m-bg-1);
		border-left: 1px solid var(--m-border);
		overflow: hidden;
		display: flex;
		flex-direction: column;
	}

	.editor-area {
		flex: 1;
		min-height: 0;
		display: flex;
	}
	/* Bottom panel host. Fixed height set via inline style by the
	   resize handler; the inner component owns its own scroll
	   surfaces (tab body, log viewer, etc.) so the host just
	   provides a flexbox slot of the requested size. */
	.bottom-host {
		flex-shrink: 0;
		min-height: 0;
		display: flex;
		flex-direction: column;
	}

	.pane-divider {
		width: 1px;
		background: var(--m-border);
		flex-shrink: 0;
	}

	.toast {
		position: fixed;
		bottom: 36px;
		right: 16px;
		background: var(--m-bg-2);
		border: 1px solid var(--m-border-strong);
		color: var(--m-fg);
		padding: 8px 12px;
		border-radius: 6px;
		box-shadow: 0 6px 24px rgba(0, 0, 0, 0.5);
		max-width: 480px;
	}
</style>
