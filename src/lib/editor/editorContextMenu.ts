// Shared right-click menu for the code editors. Mounted by both
// `Editor.svelte` and the diff view's editable right-hand pane so
// the two surfaces offer the same actions (Rename symbol, Copy
// GitHub link) with one implementation.
//
// The menu reuses `ContextMenu.svelte` portaled onto `document.body`
// — same approach as the tab strip's menu — so the popover isn't
// clipped by the editor's `overflow: hidden`.

import { mount as mountComponent, unmount } from 'svelte';
import { EditorSelection } from '@codemirror/state';
import type { EditorView } from '@codemirror/view';
import { writeText as clipboardWriteText } from '@tauri-apps/plugin-clipboard-manager';
import ContextMenu from '../components/ContextMenu.svelte';
import type { ContextMenuItem } from '../components/contextMenu';
import { ipc } from '../ipc';
import { frontendLog } from '../logs.svelte';
import { formatError } from '../protocol';
import { workspace } from '../state.svelte';
import { lspLanguageFor } from './lspLanguage';
import { triggerRename } from './lspRename';

// 1-based inclusive line range under the current selection, or the
// caret line when nothing is selected. Same off-by-one snap as the
// editor's selection publishing so a drag ending at a line start
// doesn't over-count.
function selectedLineRange(view: EditorView): { startLine: number; endLine: number } {
	const sel = view.state.selection.main;
	const fromLine = view.state.doc.lineAt(sel.from);
	const toLine = view.state.doc.lineAt(sel.to);
	const endLine = sel.to === toLine.from && toLine.number > fromLine.number ? toLine.number - 1 : toLine.number;
	return { startLine: fromLine.number, endLine };
}

async function copyToClipboard(text: string, label: string): Promise<void> {
	// Prefer the Tauri clipboard plugin: these actions fire from a
	// ContextMenu portaled onto `document.body`, which doesn't take
	// focus, and `navigator.clipboard.writeText` rejects on WebKitGTK
	// when the triggering element isn't a focused input. Fall back to
	// `navigator.clipboard` only if the plugin throws (e.g. a plain
	// browser dev build) — same pattern as CoderPanel.
	try {
		await clipboardWriteText(text);
		workspace.flash(`Copied ${label}`);
	} catch {
		try {
			await navigator.clipboard.writeText(text);
			workspace.flash(`Copied ${label}`);
		} catch {
			workspace.flash(`Could not copy ${label}`);
		}
	}
}

async function copyPermalink(view: EditorView, path: string): Promise<void> {
	const { startLine, endLine } = selectedLineRange(view);
	const label = 'GitHub link';
	try {
		const link = await ipc.fs.gitPermalink(path, startLine, endLine);
		if (link === null) {
			workspace.flash('No GitHub link (not a GitHub repo or no commits)');
			return;
		}
		await copyToClipboard(link.url, label);
	} catch (err) {
		// The clipboard write has its own flash inside `copyToClipboard`;
		// reaching here means `gitPermalink` itself threw. Surface the
		// real error instead of a generic "Could not copy" so a backend
		// failure isn't mistaken for a clipboard problem.
		frontendLog('moon-ide', 'error', `gitPermalink failed: ${formatError(err)}`);
		workspace.flash(`Could not build ${label}: ${formatError(err)}`);
	}
}

/**
 * Owns the lifecycle of one editor right-click menu. Create one per
 * editor instance, call `open` from the `contextmenu` handler, and
 * `dispose` on unmount so the portaled host can't outlive the editor.
 */
export class EditorContextMenu {
	#menu: ReturnType<typeof mountComponent> | null = null;
	#host: HTMLElement | null = null;

	dispose(): void {
		if (this.#menu) {
			void unmount(this.#menu);
			this.#menu = null;
		}
		if (this.#host) {
			this.#host.remove();
			this.#host = null;
		}
	}

	/**
	 * Open the menu at the event position for `view` / `path`.
	 * `canRename` defaults to "is this a buffer backed by an LSP
	 * server"; pass `false` to force the Rename entry greyed out
	 * (e.g. the diff view's deleted-file pane, which has no live LSP).
	 */
	open(event: MouseEvent, view: EditorView, path: string, options?: { canRename?: boolean }): void {
		event.preventDefault();
		this.dispose();

		// Place the caret at the click position unless the click lands
		// inside the existing selection, so "Rename symbol" / "Copy
		// GitHub link" target what the user actually right-clicked on
		// rather than wherever the caret happened to be.
		const pos = view.posAtCoords({ x: event.clientX, y: event.clientY });
		const sel = view.state.selection.main;
		if (pos !== null && (pos < sel.from || pos > sel.to)) {
			view.dispatch({ selection: EditorSelection.cursor(pos) });
		}

		const canRename = options?.canRename ?? lspLanguageFor(path) !== null;
		const items: ContextMenuItem[] = [
			{
				id: 'rename-symbol',
				label: 'Rename symbol',
				disabled: !canRename,
				onSelect: () => {
					triggerRename(view);
				},
			},
			{
				id: 'copy-github-link',
				label: 'Copy GitHub link',
				onSelect: () => {
					void copyPermalink(view, path);
				},
			},
		];

		const host = document.createElement('div');
		host.setAttribute('data-editor-context-menu-root', 'true');
		host.style.position = 'fixed';
		host.style.top = '0';
		host.style.left = '0';
		host.style.width = '0';
		host.style.height = '0';
		host.style.zIndex = '9999';
		document.body.appendChild(host);

		const anchorRect = { left: event.clientX, top: event.clientY, width: 0, height: 0 };
		this.#menu = mountComponent(ContextMenu, {
			target: host,
			props: {
				items,
				anchorRect,
				onClose: () => {
					this.dispose();
				},
			},
		});
		this.#host = host;
	}
}
