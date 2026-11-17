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
	<FolderBars {onPickFolder} />
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
</style>
