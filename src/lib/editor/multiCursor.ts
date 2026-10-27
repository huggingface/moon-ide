import { addCursorAbove, addCursorBelow } from '@codemirror/commands';
import type { KeyBinding } from '@codemirror/view';

// Literal Ctrl (not `Mod`) on every platform, matching the team's VS Code /
// IntelliJ muscle memory. Must be spread *before* `defaultKeymap`: on macOS
// CM binds Ctrl-Shift-Up/Down to `selectPageUp` / `selectPageDown`, and the
// first matching binding wins. Cmd-Shift-Up/Down stays select-to-doc-edge,
// and CM's own Mod-Alt-Up/Down still adds cursors too.
export const multiCursorKeymap: readonly KeyBinding[] = [
	{ key: 'Ctrl-Shift-ArrowUp', run: addCursorAbove, preventDefault: true },
	{ key: 'Ctrl-Shift-ArrowDown', run: addCursorBelow, preventDefault: true },
];
