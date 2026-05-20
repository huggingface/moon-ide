// Apply an `LspWorkspaceEdit` (the JSON shape we receive from
// `textDocument/rename`, `textDocument/codeAction`, …) to the
// frontend's view of the workspace.
//
// Open buffers get their text rewritten in place via
// `workspace.updateText`, which marks the file dirty and triggers
// our normal `didChange` debounce so the LSP catches up
// without a save.
//
// Closed files are read off disk through the workspace host,
// rewritten, and written back; the affected paths are then
// surfaced to every running server via
// `lsp.notifyFilesChanged` so a fresh
// `workspace/diagnostic/refresh` cycle catches the new text.
//
// The edit applier itself is in `lspRename.ts` (`applyEditsToText`)
// — kept there because that's where the unit tests live and the
// algorithm is the same regardless of which LSP entry-point the
// edit came from. This file is just the orchestration layer that
// matches each `documentEdits` entry against the open-buffer
// list and routes accordingly.

import { ipc } from '../ipc';
import { formatError, type LspWorkspaceEdit } from '../protocol';
import { workspace } from '../state.svelte';
import { applyEditsToText } from './lspRename';

/** Tally returned by [`applyWorkspaceEdit`] — the caller surfaces
 * a tiny status flash like "Applied fix in 3 files (2 unsaved)".
 */
export type WorkspaceEditApplyResult = {
	openCount: number;
	closedCount: number;
	failures: { path: string; error: string }[];
};

/**
 * Apply `edit` against the workspace. Returns the per-file tally
 * so the caller can surface a status flash; failures are
 * reported per-file rather than aborting the whole edit so a
 * partial apply still leaves the user better off than no apply
 * (and the SCM panel will show exactly which files landed).
 *
 * Empty `documentEdits` is a no-op that resolves to a zero
 * tally — the calling lint-tooltip action filters those out
 * before showing the entry, but defending here too means a
 * future caller can pass through unfiltered without surprises.
 */
export async function applyWorkspaceEdit(edit: LspWorkspaceEdit): Promise<WorkspaceEditApplyResult> {
	const result: WorkspaceEditApplyResult = {
		openCount: 0,
		closedCount: 0,
		failures: [],
	};
	const closedPaths: string[] = [];
	for (const doc of edit.documentEdits) {
		if (doc.path.length === 0 || doc.edits.length === 0) {
			continue;
		}
		const openFile = workspace.openFiles.find((f) => f.path === doc.path);
		if (openFile) {
			const nextText = applyEditsToText(openFile.text, doc.edits);
			if (nextText !== openFile.text) {
				workspace.updateText(doc.path, nextText);
			}
			result.openCount++;
			continue;
		}
		try {
			const read = await ipc.fs.readFile(doc.path);
			const nextText = applyEditsToText(read.text, doc.edits);
			if (nextText !== read.text) {
				await ipc.fs.writeFile(doc.path, nextText);
			}
			closedPaths.push(doc.path);
			result.closedCount++;
		} catch (err) {
			result.failures.push({
				path: doc.path,
				error: formatError(err),
			});
		}
	}
	if (closedPaths.length > 0) {
		try {
			await ipc.lsp.notifyFilesChanged(closedPaths);
		} catch {
			// Best-effort: a server that disconnected between
			// the apply and the notify isn't a user-facing
			// failure — diagnostics will refresh on the next
			// open / window-focus / didChange anyway.
		}
	}
	return result;
}
