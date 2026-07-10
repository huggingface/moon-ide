// Synthetic path scheme for the per-commit diff pseudo-tab. Each
// commit gets its own tab keyed on `commit://<40-char-sha>`, so the
// path carries the SHA (unlike the singleton `review://default-branch`
// whose data already lives on `WorkspaceState`).
//
// Same synthetic-prefix-can't-collide-with-real-paths trick as
// `untitled:` and `review://` — workspace paths never start with
// `commit://`, so `isCommitPath` is enough to gate everything that
// touches a real path via `isSyntheticBufferPath`.

export function commitPath(sha: string): string {
	return `commit://${sha}`;
}

export function isCommitPath(path: string): boolean {
	return path.startsWith('commit://');
}

// Extract the 40-char SHA from a `commit://<sha>` path. Returns null
// for malformed paths (the routing / state code treats null as "not
// a commit tab" rather than erroring).
export function shaFromCommitPath(path: string): string | null {
	if (!path.startsWith('commit://')) {
		return null;
	}
	const sha = path.slice('commit://'.length);
	return sha.length === 40 && /^[0-9a-f]+$/.test(sha) ? sha : null;
}
