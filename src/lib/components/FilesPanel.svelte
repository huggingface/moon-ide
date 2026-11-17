<script lang="ts">
	// The file tree, as an optional panel at the window's right edge.
	import FileTree from './FileTree.svelte';
	import { workspace } from '../state.svelte';

	// Esc inside the panel yanks focus back to the active editor —
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
<div class="files-panel" data-region="files" tabindex="-1" onkeydown={onKeyDown}>
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

<style>
	.files-panel {
		flex: 1;
		min-height: 0;
		min-width: 0;
		overflow: hidden;
		padding: var(--m-space-2) 0 var(--m-space-1);
		position: relative;
		outline: none;
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
