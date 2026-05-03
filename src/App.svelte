<script lang="ts">
	import { onMount } from 'svelte';
	import { open } from '@tauri-apps/plugin-dialog';
	import Sidebar from './lib/components/Sidebar.svelte';
	import EditorPane from './lib/components/EditorPane.svelte';
	import StatusBar from './lib/components/StatusBar.svelte';
	import Splash from './lib/components/Splash.svelte';
	import Welcome from './lib/components/Welcome.svelte';
	import CommandPalette from './lib/components/CommandPalette.svelte';
	import ChatPanel from './lib/components/ChatPanel.svelte';
	import BottomPanel from './lib/components/BottomPanel.svelte';
	import { workspace } from './lib/state.svelte';
	import { slack } from './lib/slack.svelte';
	import { bottomPanel } from './lib/bottomPanel.svelte';
	import { palette, reloadWindow } from './lib/commands.svelte';
	import { cycleFocus } from './lib/focus';
	import { ipc } from './lib/ipc';

	let sidebarWidth = $state(280);
	let chatWidth = $state(320);
	let resizing = $state(false);
	let resizingChat = $state(false);
	let resizingBottom = $state(false);

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
				palette.show('search');
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
			if (!event.shiftKey && key === 'l') {
				// Browsers usually grab Ctrl+L for the address bar — in
				// the Tauri webview there's no address bar, so it's
				// free. Echoes Cursor's "Ctrl+L = open chat" muscle
				// memory, which is the chat panel users will reach for.
				event.preventDefault();
				slack.togglePanel();
				return;
			}
			if (!event.shiftKey && key === 'j') {
				// VSCode-style "show panel" toggle. Picks up service
				// log streams (and eventually terminals) — both live
				// in the bottom panel. We swallow the event regardless
				// of focus so the user can hit it from anywhere.
				event.preventDefault();
				bottomPanel.toggle();
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
		window.addEventListener('keydown', onKey);
		return () => window.removeEventListener('keydown', onKey);
	});

	async function hydrate() {
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

	function startChatResize(event: PointerEvent) {
		// Drag direction is mirrored vs. the sidebar handle: dragging the
		// chat handle left grows the panel (it's on the right edge of
		// the editor area).
		resizingChat = true;
		const startX = event.clientX;
		const startW = chatWidth;

		const onMove = (e: PointerEvent) => {
			const next = startW - (e.clientX - startX);
			chatWidth = Math.max(240, Math.min(640, next));
		};
		const onUp = () => {
			resizingChat = false;
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
	     flashes "Open folder" under dark themes on every launch of a
	     project that was already open. -->
	<Splash />
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
		{#if slack.panelVisible}
			<div
				class="resize chat-resize"
				class:active={resizingChat}
				role="separator"
				aria-orientation="vertical"
				aria-label="Resize chat panel"
				tabindex="-1"
				onpointerdown={startChatResize}
			></div>
			<aside class="chat" style:width="{chatWidth}px">
				<ChatPanel />
			</aside>
		{/if}
	</div>
	<StatusBar />
	<CommandPalette />
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

	.chat {
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
