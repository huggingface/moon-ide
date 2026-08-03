import { diff as rawDiff, type Change } from '@codemirror/merge';

/**
 * "Hide whitespace" for the merge surfaces (review tab, single-file
 * diff). Reindent-only churn — an agent moving a block a level
 * deeper, a formatter flipping tabs ↔ spaces — otherwise paints
 * every touched line as changed, which buries the real edit the
 * reviewer is looking for.
 *
 * The filter works at the diff-algorithm level rather than as a
 * decoration on top: a `Change` whose A and B texts differ only in
 * whitespace characters is dropped before chunks are ever built, so
 * the line-number tints, the per-character highlights, the
 * collapse-unchanged spacers, and next/prev-chunk navigation all
 * agree on what "changed" means. Whitespace inside string literals
 * is not treated specially (same trade-off as `git diff -w`): a
 * change is hidden only when *nothing but* whitespace differs.
 *
 * MergeView asks its diff override for chunk-relative slices of A
 * and B, so the filter can't compare offsets — it compares the
 * substrings each change actually spans.
 */
export function changesEqualModuloWhitespace(a: string, b: string, changes: readonly Change[]): readonly Change[] {
	return changes.filter((change) => {
		const fromA = a.slice(change.fromA, change.toA);
		const fromB = b.slice(change.fromB, change.toB);
		// Strip *all* whitespace runs from both sides, then compare:
		// two texts that reduce to the same non-whitespace sequence
		// differ only in indentation / spacing and are hidden.
		return fromA.replace(/\s+/g, '') !== fromB.replace(/\s+/g, '');
	});
}

/**
 * The `DiffConfig` the merge surfaces pass when the toggle is on.
 * Wraps the raw diff (same base algorithm we use with the toggle
 * off — see `DiffView.svelte` for why `presentableDiff` is
 * avoided) and drops whitespace-only changes from its output.
 */
export function diffConfigIgnoringWhitespace(): { override: (a: string, b: string) => readonly Change[] } {
	return {
		override: (a, b) => changesEqualModuloWhitespace(a, b, rawDiff(a, b)),
	};
}
