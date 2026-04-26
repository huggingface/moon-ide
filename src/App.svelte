<script lang="ts">
	import { onMount } from 'svelte';
	import { open } from '@tauri-apps/plugin-dialog';
	import Sidebar from './lib/components/Sidebar.svelte';
	import EditorPane from './lib/components/EditorPane.svelte';
	import StatusBar from './lib/components/StatusBar.svelte';
	import Welcome from './lib/components/Welcome.svelte';
	import CommandPalette from './lib/components/CommandPalette.svelte';
	import { workspace } from './lib/state.svelte';
	import { palette, reloadWindow } from './lib/commands.svelte';
	import { cycleFocus } from './lib/focus';
	import { ipc } from './lib/ipc';

	let sidebarWidth = $state(280);
	let resizing = $state(false);

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
		const ws = await ipc.workspace.active();
		if (ws) {
			workspace.workspace = ws;
			await workspace.loadPaths();
		}
		// Always restore app state — theme applies even with no workspace
		// (the welcome screen still respects the saved theme).
		await workspace.restoreAppState();
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
</script>

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
	</main>
</div>
<StatusBar />
<CommandPalette />
{#if workspace.toast}
	<div class="toast" role="status">{workspace.toast}</div>
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

	.editor-area {
		flex: 1;
		min-height: 0;
		display: flex;
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
