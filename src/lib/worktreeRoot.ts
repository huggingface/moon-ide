// The project a folder belongs to: a worktree folder walks its
// `parentPath` chain up to the first non-worktree folder. Worktrees
// don't nest any more (ADR 0093), but older worktree-of-worktree
// checkouts still exist, and resolving only one hop filed their
// sessions under the middle worktree. Mirrors the backend's
// `coder_root_of`.

import type { WorkspaceFolder } from './protocol';

export function worktreeRootPath(path: string, folders: readonly WorkspaceFolder[]): string {
	let current = path;
	for (let hop = 0; hop < 8; hop++) {
		const folder = folders.find((f) => f.path === current);
		if (folder === undefined || folder.origin.kind !== 'worktree') {
			return current;
		}
		const parent = folder.origin.parentPath;
		if (!folders.some((f) => f.path === parent)) {
			return current;
		}
		current = parent;
	}
	return current;
}
