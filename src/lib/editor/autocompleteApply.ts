import { EditorSelection, type Text } from '@codemirror/state';
import type { EditorView } from '@codemirror/view';
import { ipc } from '../ipc';
import { formatError } from '../protocol';
import { filePathFacet } from './lsp';
import { isUntitledPath, workspace } from '../state.svelte';

function normalizeFsPath(p: string): string {
	return p.replace(/\\/g, '/');
}

function isAbsoluteFsPath(p: string): boolean {
	return p.startsWith('/') || /^[A-Za-z]:/.test(p);
}

/**
 * Paths in [`OpenFile`] are **folder-relative** (e.g. `server/lib/Utils.ts`). Only
 * some entry points use absolute paths. Resolve to a forward-slash path for
 * `next_edit_complete`, preferring the longest workspace root prefix when absolute.
 */
function relativePathForNextEdit(filePath: string): string | null {
	const normalizedFile = normalizeFsPath(filePath);
	const ws = workspace.workspace;
	if (!ws || ws.folders.length === 0) {
		return null;
	}

	const stripRootPrefix = (root: string): string | null => {
		const r = normalizeFsPath(root).replace(/\/+$/, '');
		const prefix = `${r}/`;
		if (normalizedFile === r) {
			return '';
		}
		if (normalizedFile.startsWith(prefix)) {
			const rel = normalizedFile.slice(prefix.length);
			if (rel.includes('..')) {
				return null;
			}
			return rel;
		}
		return null;
	};

	if (isAbsoluteFsPath(normalizedFile)) {
		const sorted = ws.folders.toSorted((a, b) => b.path.length - a.path.length);
		for (const f of sorted) {
			const rel = stripRootPrefix(f.path);
			if (rel !== null && rel.length > 0) {
				return rel;
			}
		}
		return null;
	}

	if (normalizedFile.includes('..')) {
		return null;
	}
	const active = workspace.activeFolderPath;
	if (!active) {
		return null;
	}
	const activeNorm = normalizeFsPath(active);
	if (!ws.folders.some((f) => normalizeFsPath(f.path) === activeNorm)) {
		return null;
	}
	if (normalizedFile.length === 0) {
		return null;
	}
	return normalizedFile;
}

/** Logical lines for merging (matches per-line `.text` / Rust `lines()`). */
function linesFromReplacement(s: string): string[] {
	const t = s.replace(/\r\n/g, '\n');
	const parts = t.split('\n');
	if (parts.length > 0 && t.endsWith('\n')) {
		parts.pop();
	}
	return parts;
}

/** Position after `lineNum`'s line break, or EOF when `lineNum` is the last line. */
function positionAfterLine(doc: Text, lineNum: number): number {
	if (lineNum >= doc.lines) {
		return doc.line(doc.lines).to;
	}
	return doc.line(lineNum + 1).from;
}

function mapCaretInsideReplace(anchor: number, midFrom: number, midTo: number, insertLen: number): number {
	const oldMiddleLen = midTo - midFrom;
	if (anchor <= midFrom) {
		return anchor;
	}
	if (anchor >= midTo) {
		return anchor + insertLen - oldMiddleLen;
	}
	if (oldMiddleLen === 0) {
		return midFrom + insertLen;
	}
	const t = (anchor - midFrom) / oldMiddleLen;
	return midFrom + Math.round(t * insertLen);
}

function computeMergedPatch(
	doc: Text,
	fromLine0: number,
	toLine0: number,
	replacement: string,
	anchor: number,
): { from: number; to: number; insert: string; selection: ReturnType<typeof EditorSelection.cursor> } {
	const cmStart = fromLine0 + 1;
	const cmEnd = toLine0 + 1;
	const insertNorm = replacement.replace(/\r\n/g, '\n');

	const fullFrom = doc.line(cmStart).from;
	const fullTo = positionAfterLine(doc, cmEnd);
	const fullCaret = mapCaretInsideReplace(anchor, fullFrom, fullTo, insertNorm.length);

	if (cmStart > doc.lines || cmEnd > doc.lines || cmStart > cmEnd) {
		return {
			from: fullFrom,
			to: fullTo,
			insert: insertNorm,
			selection: EditorSelection.cursor(fullCaret),
		};
	}

	const oldLines: string[] = [];
	for (let i = cmStart; i <= cmEnd; i++) {
		oldLines.push(doc.line(i).text);
	}
	const newLines = linesFromReplacement(insertNorm);

	let prefix = 0;
	while (prefix < oldLines.length && prefix < newLines.length && oldLines[prefix] === newLines[prefix]) {
		prefix++;
	}

	let suffix = 0;
	while (
		suffix < oldLines.length - prefix &&
		suffix < newLines.length - prefix &&
		oldLines[oldLines.length - 1 - suffix] === newLines[newLines.length - 1 - suffix]
	) {
		suffix++;
	}

	if (prefix + suffix > oldLines.length) {
		return {
			from: fullFrom,
			to: fullTo,
			insert: insertNorm,
			selection: EditorSelection.cursor(fullCaret),
		};
	}

	const midStartLine = cmStart + prefix;
	const midEndLine = cmEnd - suffix;

	if (midStartLine > midEndLine) {
		const insertLine = Math.min(cmStart + prefix, doc.lines);
		const pos = doc.line(insertLine).from;
		let insert = newLines.slice(prefix, newLines.length - suffix).join('\n');
		if (insert.length > 0 && insert.includes('\n') && !insert.endsWith('\n')) {
			insert += '\n';
		}
		const caret = mapCaretInsideReplace(anchor, pos, pos, insert.length);
		return {
			from: pos,
			to: pos,
			insert,
			selection: EditorSelection.cursor(Math.max(pos, Math.min(pos + insert.length, caret))),
		};
	}

	const midFrom = doc.line(midStartLine).from;
	const midTo = positionAfterLine(doc, midEndLine);
	const oldMiddleStr = doc.sliceString(midFrom, midTo);
	let insert = newLines.slice(prefix, newLines.length - suffix).join('\n');
	if (oldMiddleStr.endsWith('\n') && insert.length > 0 && !insert.endsWith('\n')) {
		insert += '\n';
	}
	const caret = mapCaretInsideReplace(anchor, midFrom, midTo, insert.length);

	return {
		from: midFrom,
		to: midTo,
		insert,
		selection: EditorSelection.cursor(Math.max(midFrom, Math.min(midFrom + insert.length, caret))),
	};
}

export type AutocompleteApplyOutcome = 'applied' | 'skipped' | 'error';

/**
 * Calls the local autocomplete model and patches the 21-line window in the editor.
 * Does not use CodeMirror's completion UI; Ctrl+Space stays LSP-only.
 */
export async function applyAutocompleteFromEditorView(editorView: EditorView): Promise<AutocompleteApplyOutcome> {
	if (workspace.nextEditProbe?.kind !== 'ready') {
		workspace.flash('Autocomplete needs a running model (status bar).');
		return 'skipped';
	}
	const path = editorView.state.facet(filePathFacet);
	if (!path || isUntitledPath(path)) {
		workspace.flash('Autocomplete needs a saved file in the open folder.');
		return 'skipped';
	}
	const rel = relativePathForNextEdit(path);
	if (!rel || rel.includes('..')) {
		workspace.flash('Autocomplete only works on files inside a workspace folder.');
		return 'skipped';
	}
	const anchor = editorView.state.selection.main.head;
	const line = editorView.state.doc.lineAt(anchor);
	const cursorLine = line.number - 1;
	const head = workspace.headByPath.get(path);
	const headText = head === undefined || head === null ? null : head;

	workspace.beginAutocompleteRequest();
	try {
		const result = await ipc.nextEdit.complete({
			baseUrl: workspace.nextEditEffectiveHttpBase(),
			relativePath: rel,
			cursorLine,
			documentText: editorView.state.doc.toString(),
			headText,
		});
		const patch = computeMergedPatch(
			editorView.state.doc,
			result.from_line,
			result.to_line,
			result.replacement,
			anchor,
		);
		if (patch.from === patch.to && patch.insert.length === 0) {
			return 'applied';
		}
		editorView.dispatch({
			changes: { from: patch.from, to: patch.to, insert: patch.insert },
			selection: patch.selection,
		});
		return 'applied';
	} catch (e) {
		workspace.flash(`Autocomplete failed: ${formatError(e)}`);
		return 'error';
	} finally {
		workspace.endAutocompleteRequest();
	}
}
