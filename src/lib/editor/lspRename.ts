// F2 rename via LSP.
//
// Flow:
//
// 1. User parks the caret on an identifier and presses F2.
// 2. The keymap captures it, reads the word at the caret, fires
//    `textDocument/prepareRename` to ask the server whether the
//    cursor sits on a renameable symbol. Servers that say "no"
//    (cursor on punctuation, keyword, string literal) trigger a
//    quiet flash and we bail.
// 3. A docked panel opens at the top of the editor with an input
//    prefilled with the server's placeholder (typically the
//    existing name). The input auto-focuses + selects so the user
//    can immediately overwrite.
// 4. Enter → call `textDocument/rename`, get back a
//    `LspWorkspaceEdit`, and apply it:
//      - Open buffers update through `workspace.updateText`,
//        which marks the file dirty and schedules a debounced
//        `didChange`. CodeMirror's reactive `$effect` reconciles
//        the buffer text into the editor view.
//      - Closed files get their on-disk bytes read, edits
//        applied in reverse offset order, then written back
//        through the active folder's host. After the writes land
//        we fire `lsp_notify_files_changed` so the server
//        invalidates its cached copies and re-pulls diagnostics.
// 5. Escape (or clicking Cancel) dismisses the panel without
//    mutating anything.
//
// We deliberately do not auto-save the affected open buffers —
// leaving them dirty matches VSCode's behaviour and gives the
// user a clear "review then Ctrl+S" path. The SCM panel and tab
// strip surface the dirty state.

import type { Extension } from '@codemirror/state';
import { Prec, StateEffect, StateField } from '@codemirror/state';
import { EditorView, keymap, showPanel, type Panel } from '@codemirror/view';
import { ipc } from '../ipc';
import { workspace } from '../state.svelte';
import type { LspPosition, LspTextEdit, LspWorkspaceEdit } from '../protocol';
import { filePathFacet } from './lsp';
import { lspLanguageFor } from './lspLanguage';

/**
 * State payload while the rename input is open. `null` means
 * the panel is closed. `position` is the *trigger* position
 * (where the user hit F2) — `textDocument/rename` is keyed on
 * that, not on the prepare-range, so a stale buffer mutation
 * between prepare + rename doesn't break the second call.
 */
type RenameState = {
	path: string;
	languageId: string;
	position: LspPosition;
	placeholder: string;
};

const openRenameEffect = StateEffect.define<RenameState>();
const closeRenameEffect = StateEffect.define();

const renameField = StateField.define<RenameState | null>({
	create: () => null,
	update: (value, tr) => {
		for (const eff of tr.effects) {
			if (eff.is(openRenameEffect)) {
				return eff.value;
			}
			if (eff.is(closeRenameEffect)) {
				return null;
			}
		}
		// Any doc edit while the panel is open closes it — the
		// user typing in the buffer behind the panel is the
		// signal that they've moved on (the panel itself doesn't
		// receive doc edits; it owns its own input element).
		if (value !== null && tr.docChanged) {
			return null;
		}
		return value;
	},
	provide: (f) =>
		showPanel.from(f, (active) => {
			if (active === null) {
				return null;
			}
			return (view: EditorView): Panel => buildPanel(view, active);
		}),
});

function buildPanel(view: EditorView, initial: RenameState): Panel {
	const dom = document.createElement('div');
	dom.className = 'cm-lsp-rename';

	const label = document.createElement('span');
	label.className = 'cm-lsp-rename-label';
	label.textContent = `Rename '${initial.placeholder}' to:`;

	const input = document.createElement('input');
	input.type = 'text';
	input.className = 'cm-lsp-rename-input';
	input.value = initial.placeholder;
	input.spellcheck = false;

	const apply = document.createElement('button');
	apply.type = 'button';
	apply.className = 'cm-lsp-rename-apply';
	apply.textContent = 'Rename';

	const cancel = document.createElement('button');
	cancel.type = 'button';
	cancel.className = 'cm-lsp-rename-cancel';
	cancel.textContent = 'Cancel';

	dom.append(label, input, apply, cancel);

	let running = false;

	const close = () => {
		view.dispatch({ effects: closeRenameEffect.of(null) });
		view.focus();
	};

	const submit = () => {
		if (running) {
			return;
		}
		const newName = input.value.trim();
		if (newName.length === 0 || newName === initial.placeholder) {
			close();
			return;
		}
		running = true;
		apply.disabled = true;
		input.disabled = true;
		void runRename(initial, newName, view).finally(() => {
			running = false;
			// The panel is already closed by `runRename` on
			// success; on error we leave it open with the input
			// re-enabled so the user can retry.
			apply.disabled = false;
			input.disabled = false;
		});
	};

	input.addEventListener('keydown', (event) => {
		if (event.key === 'Enter') {
			event.preventDefault();
			submit();
			return;
		}
		if (event.key === 'Escape') {
			event.preventDefault();
			close();
		}
	});

	apply.addEventListener('click', () => {
		submit();
	});
	cancel.addEventListener('click', () => {
		close();
	});

	return {
		dom,
		top: true,
		mount: () => {
			// Focus + select so the user can either confirm the
			// existing name (Enter, no-op) or immediately
			// overwrite. `requestAnimationFrame` lets CM finish
			// inserting the panel into the DOM before we steal
			// focus — without it, focus can land on the input
			// before the panel is reachable, then bounce back
			// to the editor.
			requestAnimationFrame(() => {
				input.focus();
				input.select();
			});
		},
	};
}

async function runRename(state: RenameState, newName: string, view: EditorView): Promise<void> {
	let edit: LspWorkspaceEdit;
	try {
		edit = await ipc.lsp.rename(state.path, state.languageId, state.position, newName);
	} catch (err) {
		workspace.flash(`Rename failed: ${formatErr(err)}`);
		return;
	}
	if (edit.documentEdits.length === 0) {
		workspace.flash('Rename: server returned no edits');
		view.dispatch({ effects: closeRenameEffect.of(null) });
		view.focus();
		return;
	}
	let openCount = 0;
	let closedCount = 0;
	const closedPaths: string[] = [];
	for (const doc of edit.documentEdits) {
		// Empty `path` means the URI was outside the active
		// folder — translation already dropped it from the list,
		// but defend in depth so a future protocol change can't
		// silently corrupt random files.
		if (doc.path.length === 0 || doc.edits.length === 0) {
			continue;
		}
		const openFile = workspace.openFiles.find((f) => f.path === doc.path);
		if (openFile) {
			const nextText = applyEditsToText(openFile.text, doc.edits);
			if (nextText !== openFile.text) {
				workspace.updateText(doc.path, nextText);
			}
			openCount++;
			continue;
		}
		try {
			const read = await ipc.fs.readFile(doc.path);
			const nextText = applyEditsToText(read.text, doc.edits);
			if (nextText !== read.text) {
				await ipc.fs.writeFile(doc.path, nextText);
			}
			closedPaths.push(doc.path);
			closedCount++;
		} catch (err) {
			workspace.flash(`Rename: failed to update ${doc.path}: ${formatErr(err)}`);
			// Best-effort: continue with the rest. Partial
			// rename is better than no rename, and the SCM
			// panel will show the user exactly which files
			// landed.
		}
	}
	if (closedPaths.length > 0) {
		// Tell every running server that these files changed on
		// disk so it can invalidate caches and re-publish
		// diagnostics. Open buffers already routed through
		// `updateText` → `didChange`, so they're up-to-date on
		// the server's view; no need to double-notify.
		try {
			await ipc.lsp.notifyFilesChanged(closedPaths);
		} catch {
			// Best-effort: a server that disconnected between
			// the rename and the notify isn't a user-facing
			// failure — diagnostics will refresh on the next
			// open / window-focus / didChange anyway.
		}
	}
	const total = openCount + closedCount;
	const fileWord = total === 1 ? 'file' : 'files';
	const dirtyHint = openCount > 0 ? ' (unsaved — Ctrl+S to commit)' : '';
	workspace.flash(`Renamed '${state.placeholder}' → '${newName}' in ${total} ${fileWord}${dirtyHint}`);
	view.dispatch({ effects: closeRenameEffect.of(null) });
	view.focus();
}

function formatErr(err: unknown): string {
	if (err instanceof Error) {
		return err.message;
	}
	return String(err);
}

/**
 * Apply a server's `LspTextEdit[]` to `original`. Edits inside
 * one document never overlap per the LSP spec, so we sort
 * descending by start position and slice in-place — earlier
 * offsets aren't affected by later replacements.
 */
export function applyEditsToText(original: string, edits: readonly LspTextEdit[]): string {
	const lineStarts = lineStartsOf(original);
	const sorted = edits.toSorted((a, b) => {
		if (a.range.start.line !== b.range.start.line) {
			return b.range.start.line - a.range.start.line;
		}
		return b.range.start.character - a.range.start.character;
	});
	let text = original;
	for (const edit of sorted) {
		const from = offsetForPosition(original.length, lineStarts, edit.range.start);
		const to = offsetForPosition(original.length, lineStarts, edit.range.end);
		text = text.slice(0, from) + edit.newText + text.slice(to);
	}
	return text;
}

function lineStartsOf(text: string): number[] {
	const starts: number[] = [0];
	for (let i = 0; i < text.length; i++) {
		// Codepoint 10 (`\n`) is the only line terminator LSP
		// positions are spec'd against. Servers normalise
		// `\r\n` to `\n` when computing positions; we mirror
		// that by ignoring `\r`.
		if (text.charCodeAt(i) === 10) {
			starts.push(i + 1);
		}
	}
	return starts;
}

function offsetForPosition(textLength: number, lineStarts: readonly number[], pos: LspPosition): number {
	if (pos.line < 0) {
		return 0;
	}
	const start = lineStarts[pos.line];
	if (start === undefined) {
		return textLength;
	}
	return start + pos.character;
}

/**
 * F2 keymap entry. Returns `true` only when we've consumed the
 * key — a server-less buffer (markdown, JSON, log files) falls
 * through to CM's default F2 binding (which is unbound, so it's
 * a no-op).
 */
function triggerRename(view: EditorView): boolean {
	const path = view.state.facet(filePathFacet);
	if (path === null) {
		return false;
	}
	const languageId = lspLanguageFor(path);
	if (languageId === null) {
		return false;
	}
	const head = view.state.selection.main.head;
	const word = view.state.wordAt(head);
	if (!word) {
		workspace.flash('Rename: cursor is not on an identifier');
		return true;
	}
	const fallback = view.state.sliceDoc(word.from, word.to);
	const position = lspPositionAt(view, head);
	void (async () => {
		let prepared;
		try {
			prepared = await ipc.lsp.prepareRename(path, languageId, position, fallback);
		} catch (err) {
			workspace.flash(`Rename unavailable: ${formatErr(err)}`);
			return;
		}
		const placeholder = prepared?.placeholder ?? fallback;
		if (prepared === null && !looksRenameable(fallback)) {
			// Server explicitly said no, and the word isn't a
			// plausible identifier (punctuation, whitespace
			// span). Bail with a hint.
			workspace.flash('Rename: not a renameable symbol');
			return;
		}
		view.dispatch({
			effects: openRenameEffect.of({
				path,
				languageId,
				position,
				placeholder,
			}),
		});
	})();
	return true;
}

function lspPositionAt(view: EditorView, offset: number): LspPosition {
	const line = view.state.doc.lineAt(offset);
	return { line: line.number - 1, character: offset - line.from };
}

/**
 * Cheap "looks like an identifier" filter — used to decide
 * whether to surface a "not renameable" hint when the server
 * declined to prepare. A bare ASCII identifier-ish word still
 * tries the full `rename` request (some servers don't implement
 * `prepareRename` and only respond on the actual `rename`),
 * while obvious non-identifiers (whitespace runs, punctuation)
 * get a clean "no" immediately.
 */
function looksRenameable(word: string): boolean {
	return /^[A-Za-z_$][\w$]*$/.test(word);
}

export function lspRenameExtension(): Extension {
	// `Prec.high` so F2 doesn't get shadowed by any default-
	// precedence binding a language extension might install
	// (CM's defaults don't claim F2, but we're future-proofing).
	return [
		renameField,
		Prec.high(
			keymap.of([
				{
					key: 'F2',
					run: triggerRename,
				},
			]),
		),
	];
}
