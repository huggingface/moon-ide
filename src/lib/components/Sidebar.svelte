<script lang="ts">
	import FileTree from './FileTree.svelte';
	import FolderBars from './FolderBars.svelte';
	import ScmPanel from './ScmPanel.svelte';
	import { workspace } from '../state.svelte';

	type Props = {
		onPickFolder: () => void | Promise<void>;
	};
	let { onPickFolder }: Props = $props();

	let sidebar: HTMLDivElement | undefined = $state();

	// Pull focus into the sidebar when WorkspaceState bumps the tick
	// (F6 cycle, Ctrl+0). When a folder is active, FileTree.svelte
	// owns the focus pull — it has access to Pierre's instance and
	// can pierce the shadow DOM where the tree rows actually live.
	// Sidebar steps in only when there's no tree to focus into (no
	// folder is active), where the fallback target is FolderBars'
	// `+ Add folder` button — discoverable and keyboard-reachable
	// from a single F6 hop.
	$effect(() => {
		const tick = workspace.sidebarFocusTick;
		if (tick === 0 || workspace.activeFolder !== null) {
			return;
		}
		queueMicrotask(() => {
			const addBtn = sidebar?.querySelector<HTMLButtonElement>('[data-folder-add-button]');
			addBtn?.focus();
		});
	});

	// Esc inside the sidebar yanks focus back to the active editor —
	// VSCode's behavior. Skipped while the user is typing in Pierre's
	// search input (or any other input/textarea) so Esc still serves
	// its native "clear/close" role there.
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
	<FolderBars {onPickFolder} />
	{#if workspace.activeFolder}
		<!-- SCM panel: branch label + commit-message input. Hidden
		     when no folder is bound — there's nothing to commit. We
		     re-mount per active folder so the input draft and the
		     branch label can't leak across folder switches. -->
		{#key workspace.activeFolderPath}
			<ScmPanel />
		{/key}
	{/if}
	<div class="tree">
		{#if workspace.activeFolder}
			<!-- Re-mount on folder switch. Per-folder tree state
			     (expansion, scroll position) is intentionally not
			     preserved across folder swaps in 2.5 — adding
			     cross-switch tree memoisation is a Phase 7 follow-up.
			     The per-folder *tab* state is preserved through
			     `WorkspaceState.folderStates` and swaps in lock-step.

			     Both trees stay mounted simultaneously so toggling the
			     SCM filter swap is instant and doesn't lose either
			     view's expansion / scroll memory. We use absolute
			     positioning + a `visibility` toggle rather than
			     `display: none` because Pierre's virtualizer needs a
			     measurable container height to decide which rows to
			     render — `display: none` would leave it staring at a
			     0-height box on the first re-show. -->
			{#key workspace.activeFolderPath}
				<div class="tree-stack">
					<div class="tree-pane" class:hidden={workspace.scmFilterOn}>
						<FileTree mode="all" />
					</div>
					<div class="tree-pane" class:hidden={!workspace.scmFilterOn}>
						<FileTree mode="changes" />
					</div>
				</div>
			{/key}
		{/if}
	</div>
</div>

<style>
	.sidebar {
		display: flex;
		flex-direction: column;
		height: 100%;
		min-width: 0;
		outline: none;
	}
	.tree {
		flex: 1;
		min-height: 0;
		overflow: hidden;
		padding: 8px 0 4px;
		position: relative;
	}
	.tree-stack {
		position: relative;
		height: 100%;
		min-height: 0;
	}
	.tree-pane {
		position: absolute;
		inset: 0;
		display: flex;
		min-height: 0;
	}
	.tree-pane.hidden {
		visibility: hidden;
		/* Pointer events disabled on the hidden pane so Pierre's
		   own keyboard / mouse handlers in the inactive tree can't
		   accidentally swallow events targeted at the visible one. */
		pointer-events: none;
	}
</style>
