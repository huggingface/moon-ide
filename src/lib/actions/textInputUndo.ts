import type { Action } from 'svelte/action';

/**
 * Wire **Ctrl+Z**, **Ctrl+Shift+Z**, and **Ctrl+Y** on an
 * `<input>` or `<textarea>` to the element's native undo stack
 * via `document.execCommand('undo' | 'redo')`.
 *
 * Why this exists: webkit2gtk inside Tauri keeps a per-element
 * undo stack but does **not** bind those keystrokes to it the way
 * Chromium does, so without this action a focused `<textarea>`
 * receives the `keydown` and does nothing — typing is recorded in
 * the stack, the inspector's Undo menu item works, but Ctrl+Z is
 * silent. `execCommand` walks the same stack the inspector uses
 * and, importantly, fires the matching `input` event with
 * `inputType: 'historyUndo'` / `'historyRedo'` so any framework
 * binding (Svelte's `bind:value`, a manual `oninput` listener,
 * etc.) sees the rolled-back value and stays in sync.
 *
 * The other key piece is to **not fight the textarea's undo
 * stack**: any JS write to `el.value` clears it. With Svelte 5's
 * `bind:value`, the binding effect occasionally writes back to
 * `el.value` on its own, which silently wipes the stack mid-edit.
 * This action only addresses the keystroke binding; if you also
 * see surprise undo-buffer wipes, the consumer probably needs to
 * own its DOM sync (manual `oninput` + a `$effect` that only
 * writes to `el.value` when the state has actually diverged).
 *
 * Usage:
 *
 * ```svelte
 * <textarea use:textInputUndo bind:value={message} />
 * ```
 *
 * Bubbling-phase listener with a `defaultPrevented` guard, so a
 * component-level `onkeydown` that intentionally swallowed
 * Ctrl+Z (rare, but possible) still wins. `Ctrl` here means
 * `ctrlKey || metaKey` so Cmd+Z on macOS works the same way.
 */
// Runtime narrow: `addEventListener` overloads don't propagate
// the `'keydown'` → `KeyboardEvent` mapping through the union
// element type, so the listener accepts `Event` and checks the
// actual instance. Cheap, and avoids a structural cast.
function onKeydown(event: Event): void {
	if (!(event instanceof KeyboardEvent) || event.defaultPrevented) {
		return;
	}
	const ctrl = event.ctrlKey || event.metaKey;
	if (!ctrl) {
		return;
	}
	const key = event.key.toLowerCase();
	if (key === 'z' && !event.shiftKey) {
		event.preventDefault();
		document.execCommand('undo');
		return;
	}
	if ((key === 'z' && event.shiftKey) || key === 'y') {
		event.preventDefault();
		document.execCommand('redo');
	}
}

export const textInputUndo: Action<HTMLInputElement | HTMLTextAreaElement> = (node) => {
	node.addEventListener('keydown', onKeydown);
	return {
		destroy() {
			node.removeEventListener('keydown', onKeydown);
		},
	};
};
