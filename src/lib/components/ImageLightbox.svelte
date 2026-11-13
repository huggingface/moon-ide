<script lang="ts">
	import { copyImageToClipboard } from '../copyImage';
	import { closeLightbox, lightbox } from '../imageLightbox.svelte';
	import { formatError } from '../protocol';
	import { workspace } from '../state.svelte';
	import ContextMenu from './ContextMenu.svelte';
	import type { ContextMenuItem } from './contextMenu';

	const image = $derived(lightbox.image);

	// Capture phase so Esc closes the viewer without also reaching the
	// coder panel's Esc handling (stop turn, close pop-out).
	$effect(() => {
		if (image === null) {
			return;
		}
		const onKey = (event: KeyboardEvent) => {
			if (event.key !== 'Escape') {
				return;
			}
			event.preventDefault();
			event.stopPropagation();
			if (menu !== null) {
				menu = null;
				return;
			}
			closeLightbox();
		};
		window.addEventListener('keydown', onKey, true);
		return () => window.removeEventListener('keydown', onKey, true);
	});

	// Our own menu: WebKitGTK's "Copy Image" copies a `data:` image as
	// its URL text, which pastes back as base64 instead of a picture.
	let menu = $state<{ x: number; y: number; items: ContextMenuItem[] } | null>(null);

	function onImageContextMenu(event: MouseEvent, src: string, path: string | undefined): void {
		event.preventDefault();
		const items: ContextMenuItem[] = [
			{
				id: 'copy-image',
				label: 'Copy image',
				onSelect: () => {
					copyImageToClipboard(src).then(
						() => workspace.flash('Image copied'),
						(err: unknown) => workspace.flash(`Copy failed: ${formatError(err)}`),
					);
				},
			},
		];
		if (path !== undefined) {
			items.push({ id: 'open-image', label: 'Open in editor', onSelect: () => openInEditor(path) });
		}
		menu = { x: event.clientX, y: event.clientY, items };
	}

	function openInEditor(path: string): void {
		closeLightbox();
		void workspace.openFile(path);
	}
</script>

{#if image !== null}
	<!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions -->
	<div
		class="backdrop"
		onclick={closeLightbox}
		role="dialog"
		aria-modal="true"
		aria-label="Image preview"
		tabindex="-1"
	>
		<img
			class="image"
			src={image.src}
			alt={image.alt}
			oncontextmenu={(e) => onImageContextMenu(e, image.src, image.workspacePath)}
		/>
		<div class="actions">
			{#if image.workspacePath !== undefined}
				{@const path = image.workspacePath}
				<button
					type="button"
					class="action"
					title="Open in editor"
					onclick={(e) => {
						e.stopPropagation();
						openInEditor(path);
					}}
				>
					Open in editor
				</button>
			{/if}
			<button type="button" class="action close" title="Close (Esc)" onclick={closeLightbox}>×</button>
		</div>
	</div>
	{#if menu !== null}
		<ContextMenu
			items={menu.items}
			anchorRect={{ left: menu.x, top: menu.y, width: 0, height: 0 }}
			onClose={() => (menu = null)}
		/>
	{/if}
{/if}

<style>
	.backdrop {
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
	.image {
		max-width: 100%;
		max-height: 100%;
		object-fit: contain;
		border-radius: 4px;
		box-shadow: 0 8px 32px rgba(0, 0, 0, 0.5);
	}
	.actions {
		position: absolute;
		top: 16px;
		right: 16px;
		display: flex;
		gap: 6px;
	}
	.action {
		height: 32px;
		padding: 0 10px;
		display: flex;
		align-items: center;
		justify-content: center;
		font: inherit;
		font-size: 12px;
		color: var(--m-fg);
		background: var(--m-bg-overlay);
		border: 1px solid var(--m-border);
		border-radius: 4px;
		cursor: pointer;
	}
	.action.close {
		width: 32px;
		padding: 0;
		font-size: 22px;
		line-height: 1;
	}
	.action:hover {
		background: var(--m-bg);
	}
</style>
