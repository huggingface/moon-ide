// Synthetic path scheme for the "Review changes" pseudo-tab. The
// review tab renders a stack of read-only per-file diff sections
// against the active folder's default-branch merge-base — same
// data the SCM panel paints in `compareBaseline === 'default'`
// mode, just stitched together so the user can scroll one page
// of diffs instead of opening each file separately.
//
// One path per workspace (rather than `review://<sha>` / per
// session ids): there's at most one review tab open per folder
// at any time, and the underlying merge-base / changed-file list
// is already on `WorkspaceState` and re-derives on every git
// refresh. The path is the routing hint, not a data carrier.
//
// Same `synthetic-prefix-can't-collide-with-real-paths` trick as
// `untitled:` — workspace paths never start with `review://`, so
// the check is enough to gate everything that touches a real
// path (LSP open / update / close, persistence, blame, HEAD
// fetch, editorconfig, format-on-save…). Most of those gates go
// through [`isSyntheticBufferPath`] in `state.svelte.ts` rather
// than checking review-vs-untitled separately.

export const REVIEW_PATH = 'review://default-branch';

export function isReviewPath(path: string): boolean {
	return path.startsWith('review://');
}
