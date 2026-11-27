<script lang="ts">
	import { untrack } from 'svelte';
	import FolderBars from './FolderBars.svelte';
	import InboxButton from './InboxButton.svelte';
	import ScmPanel from './ScmPanel.svelte';
	import SessionList from './SessionList.svelte';
	import { workspace } from '../state.svelte';

	type Props = {
		onPickFolder: () => void | Promise<void>;
	};
	let { onPickFolder }: Props = $props();

	let sidebar: HTMLDivElement | undefined = $state();
	let projectsPane: HTMLDivElement | undefined = $state();

	// The project list takes what it needs up to a share of the
	// sidebar, then scrolls, so the sessions below keep their room
	// however many folders are bound. The divider under it resizes
	// that share; the choice is a per-machine preference.
	const PROJECTS_SHARE_KEY = 'moon-sidebar-projects-share';
	const DEFAULT_SHARE = 0.4;
	let projectsShare = $state(readShare());

	function readShare(): number {
		const stored = Number(localStorage.getItem(PROJECTS_SHARE_KEY));
		return stored > 0.05 && stored < 0.95 ? stored : DEFAULT_SHARE;
	}

	function startResize(event: PointerEvent): void {
		if (!sidebar || !projectsPane) {
			return;
		}
		event.preventDefault();
		const handle = event.currentTarget as HTMLElement;
		handle.setPointerCapture(event.pointerId);
		const total = sidebar.getBoundingClientRect().height;
		const top = projectsPane.getBoundingClientRect().top;
		const onMove = (move: PointerEvent): void => {
			projectsShare = Math.min(0.9, Math.max(0.1, (move.clientY - top) / total));
		};
		const onUp = (): void => {
			handle.removeEventListener('pointermove', onMove);
			handle.removeEventListener('pointerup', onUp);
			localStorage.setItem(PROJECTS_SHARE_KEY, String(projectsShare));
		};
		handle.addEventListener('pointermove', onMove);
		handle.addEventListener('pointerup', onUp);
	}

	function resetShare(): void {
		projectsShare = DEFAULT_SHARE;
		localStorage.removeItem(PROJECTS_SHARE_KEY);
	}

	// Keep the active project in view when it changes from elsewhere
	// (palette, a session opening another folder).
	$effect(() => {
		void workspace.activeFolderPath;
		queueMicrotask(() => {
			projectsPane?.querySelector('.bar.active')?.scrollIntoView({ block: 'nearest' });
		});
	});

	// Pull focus into the sidebar when WorkspaceState bumps the tick
	// (F6 cycle). It lands on the sessions list — its search box, or
	// the new-session button while there is nothing to search. With
	// no folder there is no list, and the target is FolderBars'
	// `+ Add folder` button — discoverable and keyboard-reachable
	// from a single F6 hop. The tick is the sole trigger: the folder
	// is read untracked so adding or removing one doesn't steal focus.
	$effect(() => {
		const tick = workspace.sidebarFocusTick;
		if (tick === 0) {
			return;
		}
		const hasFolder = untrack(() => workspace.activeFolder !== null);
		queueMicrotask(() => {
			const target = hasFolder
				? (sidebar?.querySelector<HTMLElement>('[data-session-search]') ??
					sidebar?.querySelector<HTMLElement>('[data-session-new]'))
				: sidebar?.querySelector<HTMLElement>('[data-folder-add-button]');
			target?.focus();
		});
	});

	// Esc inside the sidebar yanks focus back to the active editor —
	// VSCode's behavior. Skipped while the user is typing in an
	// input/textarea so Esc still serves its native "clear/close"
	// role there.
	function onKeyDown(event: KeyboardEvent) {
		if (event.key !== 'Escape') {
			return;
		}
		const ae = document.activeElement;
		if (ae instanceof HTMLInputElement || ae instanceof HTMLTextAreaElement) {
			return;
		}
		event.preventDefault();
		workspace.requestEditorFocus();
	}
</script>

<!-- svelte-ignore a11y_no_static_element_interactions -->
<div class="sidebar" data-region="sidebar" tabindex="-1" bind:this={sidebar} onkeydown={onKeyDown}>
	{#if workspace.workspace}
		<InboxButton />
	{/if}
	<div class="projects-pane" bind:this={projectsPane} style:max-height="{projectsShare * 100}%">
		<FolderBars {onPickFolder} />
	</div>
	<!-- svelte-ignore a11y_no_static_element_interactions -->
	<div
		class="projects-resize"
		title="Drag to resize the project list (double-click to reset)"
		onpointerdown={startResize}
		ondblclick={resetShare}
	></div>
	{#if workspace.activeFolder}
		<!-- SCM panel: branch label + commit-message input. Hidden
		     when no folder is bound — there's nothing to commit. We
		     re-mount per active folder so the input draft and the
		     branch label can't leak across folder switches. -->
		{#key workspace.activeFolderPath}
			<ScmPanel />
		{/key}
		<SessionList />
	{/if}
</div>

<style>
	.sidebar {
		display: flex;
		flex-direction: column;
		height: 100%;
		min-width: 0;
		outline: none;
	}
	.projects-pane {
		flex: 0 0 auto;
		min-height: 64px;
		overflow-y: auto;
		scrollbar-width: thin;
	}
	.projects-resize {
		flex: none;
		height: 5px;
		margin: 0 var(--m-space-2) var(--m-space-1);
		border-top: 1px solid var(--m-border);
		cursor: row-resize;
	}
	.projects-resize:hover {
		border-top-color: var(--m-accent);
	}
</style>
