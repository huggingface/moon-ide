/** Split a unified diff into per-file sections keyed by the
 * `b/<path>` of each `diff --git` header. Shared by the branch-review
 * overlay (SessionView) and the working-changes overlay
 * (WorkspaceView). A file missing from the map means its hunk was
 * binary, a bare rename, or fell past the backend's 64 kB cap. */
export function diffSections(diff: string): Map<string, string> {
	const map = new Map<string, string>();
	if (!diff) {
		return map;
	}
	for (const part of diff.split(/^(?=diff --git )/m)) {
		if (!part.startsWith('diff --git ')) {
			continue;
		}
		const header = part.slice(0, part.indexOf('\n'));
		const m = header.match(/ b\/(.+)$/);
		if (m?.[1] !== undefined) {
			map.set(m[1], part);
		}
	}
	return map;
}
