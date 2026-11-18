// How a pull request's files are laid out for review (ADR 0118):
// which start collapsed, what is left to look at, and how much each
// one has attached to it.

import type { PrDiffFile } from './protocol';

/** Above this many files nothing starts open: the file list is the
 *  way in, and the page stays short enough to hold in the head. */
export const LARGE_PR_FILES = 8;

/** A single file changing more lines than this starts collapsed. */
export const LARGE_FILE_LINES = 400;

/** Files nobody reads line by line: lockfiles, snapshots, minified
 *  bundles. */
const GENERATED =
	/(^|\/)(Cargo\.lock|bun\.lock|bun\.lockb|package-lock\.json|pnpm-lock\.yaml|yarn\.lock|go\.sum|uv\.lock|poetry\.lock)$|\.snap$|\.min\.(js|css)$/;

/** Whether `file` starts collapsed in a pull request of `fileCount`
 *  files. The reader's own toggles and viewed marks override this. */
export function startsCollapsed(file: PrDiffFile, fileCount: number): boolean {
	if (fileCount > LARGE_PR_FILES || file.status === 'deleted' || GENERATED.test(file.path)) {
		return true;
	}
	if (file.additions === null || file.deletions === null) {
		return true;
	}
	return file.additions + file.deletions > LARGE_FILE_LINES;
}

/** The file to move to once `from` is marked viewed: the next
 *  unviewed one after it, wrapping round to those before it. `null`
 *  when everything is viewed. */
export function nextUnviewed(
	files: readonly PrDiffFile[],
	viewed: Readonly<Record<string, boolean>>,
	from: string,
): string | null {
	const start = files.findIndex((file) => file.path === from);
	for (let step = 1; step <= files.length; step++) {
		const candidate = files[(start + step) % files.length];
		if (candidate !== undefined && candidate.path !== from && !viewed[candidate.path]) {
			return candidate.path;
		}
	}
	return null;
}

/** Per path, how many things on GitHub point at it and still want
 *  attention: unresolved review threads plus moonbot's notes. */
export function attentionCounts(pr: {
	threads: readonly { path: string; resolved: boolean }[];
	minorNotes: readonly { path: string }[];
}): Map<string, number> {
	const counts = new Map<string, number>();
	const bump = (path: string) => counts.set(path, (counts.get(path) ?? 0) + 1);
	for (const thread of pr.threads) {
		if (!thread.resolved) {
			bump(thread.path);
		}
	}
	for (const note of pr.minorNotes) {
		bump(note.path);
	}
	return counts;
}

/** What a file's section header shows and does in the PR tab. */
export type FileOutline = {
	additions: number | null;
	deletions: number | null;
	attention: number;
	collapsed: boolean;
	viewed: boolean;
	onToggle: () => void;
	onViewed: (viewed: boolean) => void;
};
