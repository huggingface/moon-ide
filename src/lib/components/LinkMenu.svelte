<script lang="ts">
	// One right-click menu for every web link in the app: transcript,
	// tool rows, pull request and issue text, cards in a diff, Markdown
	// previews. Without it a link gets the webview's own menu, whose
	// "Open Link" navigates the IDE away.
	import { writeText as clipboardWriteText } from '@tauri-apps/plugin-clipboard-manager';
	import { openUrl } from '@tauri-apps/plugin-opener';
	import { openInIdeBrowser } from '../browserOpen';
	import { linkAt } from '../linkTarget';
	import { formatError } from '../protocol';
	import { workspace } from '../state.svelte';
	import ContextMenu from './ContextMenu.svelte';
	import type { ContextMenuItem } from './contextMenu';

	let menu = $state<{ items: ContextMenuItem[]; x: number; y: number } | null>(null);

	function itemsFor(href: string): ContextMenuItem[] {
		return [
			{
				id: 'open-link-ide',
				label: 'Open in IDE browser',
				onSelect: () => {
					openInIdeBrowser(href).catch((err: unknown) => {
						workspace.flash(`Couldn't open the tab: ${formatError(err)}`);
					});
				},
			},
			{ id: 'open-link', label: 'Open in system browser', onSelect: () => void openUrl(href) },
			{
				id: 'copy-link',
				label: 'Copy link',
				onSelect: () => {
					clipboardWriteText(href).then(
						() => workspace.flash('Link copied'),
						(err: unknown) => workspace.flash(`Copy failed: ${formatError(err)}`),
					);
				},
			},
		];
	}

	// In the capture phase, and stopped there: a link inside an editor
	// or a panel with a menu of its own gets this one, not both.
	function onContextMenu(event: MouseEvent) {
		const href = linkAt(event.target);
		if (href === null) {
			return;
		}
		event.preventDefault();
		event.stopPropagation();
		menu = { items: itemsFor(href), x: event.clientX, y: event.clientY };
	}
</script>

<svelte:document oncontextmenucapture={onContextMenu} />

{#if menu !== null}
	<ContextMenu
		items={menu.items}
		anchorRect={{ left: menu.x, top: menu.y, width: 0, height: 0 }}
		onClose={() => (menu = null)}
	/>
{/if}
