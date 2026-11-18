// Keeps the two `.cm-scroller`s of a split diff at the same
// horizontal offset. `@codemirror/merge` aligns the panes vertically
// (one outer scroller drives both) but each pane scrolls sideways on
// its own, so a long line could slide out of step with its
// counterpart.

/**
 * Mirror `scrollLeft` both ways. Echoes of our own writes are told
 * apart by *value*, not by a timed flag: when the two sides have
 * different `scrollWidth`s the narrower one clamps, and a flag
 * released on the next frame lets that clamped echo yank the wider
 * side back. Recording the value the write will settle at and
 * swallowing exactly that event is right whatever the frame timing.
 *
 * Returns the cleanup that removes both listeners.
 */
export function wireHorizontalScrollSync(a: HTMLElement, b: HTMLElement): () => void {
	const expected = new WeakMap<HTMLElement, number>();
	const handle = (from: HTMLElement, to: HTMLElement) => {
		const pending = expected.get(from);
		expected.delete(from);
		if (pending !== undefined && from.scrollLeft === pending) {
			return;
		}
		const toMax = Math.max(0, to.scrollWidth - to.clientWidth);
		const target = Math.min(from.scrollLeft, toMax);
		if (to.scrollLeft === target) {
			return;
		}
		expected.set(to, target);
		to.scrollLeft = target;
	};
	const onA = () => handle(a, b);
	const onB = () => handle(b, a);
	a.addEventListener('scroll', onA, { passive: true });
	b.addEventListener('scroll', onB, { passive: true });
	return () => {
		a.removeEventListener('scroll', onA);
		b.removeEventListener('scroll', onB);
	};
}
