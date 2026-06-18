<script lang="ts">
	// Advanced, deliberately tucked-away affordance on `write_file`
	// / `edit_file` tool rows: reapply the recorded edit to disk.
	// The recovery hatch for "I reset / clobbered that file and want
	// the agent's change back" without re-running the whole turn.
	//
	// Rendered only inside the expanded tool body (you have to open
	// the row first), and gated behind a one-item menu rather than a
	// bare button — `write_file` overwrites the whole file, so the
	// extra click guards against an accidental clobber. Pure
	// side-effect: the backend dispatches the call again and touches
	// disk, but never the transcript or the JSONL.
	import { mount as mountComponent, unmount } from 'svelte';
	import ContextMenu from './ContextMenu.svelte';
	import type { ContextMenuItem } from './contextMenu';
	import SettingsIcon from './icons/SettingsIcon.svelte';
	import { ipc } from '../ipc';
	import { formatError } from '../protocol';
	import { workspace } from '../state.svelte';

	interface Props {
		/** Tool-call id the backend looks the recorded args up by. */
		callId: string;
	}

	let { callId }: Props = $props();

	let busy = $state(false);

	// The menu is mounted into a portal host on `document.body`
	// rather than rendered inline: the transcript scroll container
	// clips and mis-positions a `position: fixed` popover, the same
	// reason the tab / file-tree menus portal out. Torn down on
	// close and on unmount.
	let menu: ReturnType<typeof mountComponent> | null = null;
	let menuHost: HTMLElement | null = null;

	$effect(() => {
		return () => {
			disposeMenu();
		};
	});

	function disposeMenu(): void {
		if (menu) {
			void unmount(menu);
			menu = null;
		}
		if (menuHost) {
			menuHost.remove();
			menuHost = null;
		}
	}

	async function reapply(): Promise<void> {
		if (busy) {
			return;
		}
		busy = true;
		try {
			const outcome = await ipc.coder.rerunToolCall(callId);
			workspace.flash(`Re-applied ${outcome.tool_name} to disk.`);
		} catch (err) {
			workspace.flash(`Re-apply failed: ${formatError(err)}`);
		} finally {
			busy = false;
		}
	}

	function openMenu(event: MouseEvent): void {
		event.stopPropagation();
		disposeMenu();

		const rect = (event.currentTarget as HTMLElement).getBoundingClientRect();
		const items: ContextMenuItem[] = [
			{
				id: 'reapply',
				label: 'Re-apply this edit to disk',
				title: 'Dispatch this edit again against the current file',
				onSelect: () => void reapply(),
			},
		];

		const host = document.createElement('div');
		host.setAttribute('data-tool-reapply-menu-root', 'true');
		host.style.position = 'fixed';
		host.style.top = '0';
		host.style.left = '0';
		host.style.width = '0';
		host.style.height = '0';
		host.style.zIndex = '9999';
		document.body.appendChild(host);

		menu = mountComponent(ContextMenu, {
			target: host,
			props: {
				items,
				anchorRect: { left: rect.left, top: rect.top, width: rect.width, height: rect.height },
				onClose: () => disposeMenu(),
			},
		});
		menuHost = host;
	}
</script>

<button
	type="button"
	class="reapply-cog"
	title="Re-apply this edit to disk"
	aria-label="Re-apply this edit to disk"
	disabled={busy}
	onclick={openMenu}
>
	<SettingsIcon size={13} />
</button>

<style>
	.reapply-cog {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		flex: 0 0 auto;
		padding: 2px;
		color: var(--m-fg-subtle);
		background: transparent;
		border: none;
		border-radius: 4px;
		cursor: pointer;
		opacity: 0.6;
	}
	.reapply-cog:hover:not(:disabled) {
		color: var(--m-fg);
		background: var(--m-bg-hover, color-mix(in srgb, var(--m-fg) 8%, transparent));
		opacity: 1;
	}
	.reapply-cog:disabled {
		cursor: default;
		opacity: 0.35;
	}
</style>
