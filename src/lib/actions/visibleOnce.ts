import type { Action } from 'svelte/action';

/**
 * Fire `callback` the first time the element scrolls into (or near)
 * the viewport, then stop observing. Sticky by design — once the
 * caller has been told "you're visible", we never tell them
 * "you're not": the heavy content can mount and stay mounted.
 *
 * Why this exists: `CoderPanel`'s transcript renders every row up
 * front, and each `CoderMarkdown` / `ToolBody*` instance mounts its
 * own decorated content (rendered markdown, syntax-highlighted code
 * blocks, async highlighter loads, …). On a folder swap into a
 * session with many rows, that fans out into a cascade of DOM
 * mutations + style recalculations even when most of the rows are
 * scrolled out of view. Gating the heavy render on first-visibility
 * cuts the cold-cache swap cost without changing the steady-state
 * UX: rows above the fold render immediately, rows below stay as
 * cheap placeholders until the user scrolls toward them. The
 * `rootMargin` slop means we start the render a screenful before
 * the row enters the viewport so the placeholder→content swap is
 * usually invisible during a fast scroll.
 *
 * Sticky semantics matter for two reasons: (1) we don't want a
 * mounted markdown body to tear down and re-mount when the user
 * scrolls past it (that would defeat the cache and flicker the
 * layout), and (2) `Ctrl+F` matches inside the placeholder text
 * still scroll-into-view the same way as matches inside rendered
 * markdown, and the observer correctly fires once for the
 * placeholder match.
 *
 * One shared `IntersectionObserver` covers every target — observing
 * N nodes on the same instance lets the browser compute their
 * bounding boxes in one batched layout pass instead of forcing a
 * fresh layout on each `observe()` call. With per-instance
 * observers a 70-row transcript was paying ~70 separate forced
 * layouts during the mount cascade (test-plan 0076, ship 8); the
 * shared observer collapses that to one. Callbacks are dispatched
 * via a `WeakMap` keyed by the target element so each caller still
 * sees only its own intersection.
 */
const sharedCallbacks = new WeakMap<Element, () => void>();
let sharedObserver: IntersectionObserver | null = null;

function getSharedObserver(): IntersectionObserver {
	if (sharedObserver !== null) {
		return sharedObserver;
	}
	sharedObserver = new IntersectionObserver(
		(entries, observer) => {
			for (const entry of entries) {
				if (!entry.isIntersecting) {
					continue;
				}
				const cb = sharedCallbacks.get(entry.target);
				if (cb !== undefined) {
					sharedCallbacks.delete(entry.target);
					observer.unobserve(entry.target);
					cb();
				}
			}
		},
		{
			// One screenful of slop above + below so off-screen rows
			// hydrate just before the user scrolls them in. 400px is
			// roughly two chat bubbles on a typical layout — enough
			// lead time to hide the placeholder swap on a normal
			// scroll speed.
			rootMargin: '400px',
		},
	);
	return sharedObserver;
}

export const visibleOnce: Action<HTMLElement, () => void> = (node, callback) => {
	sharedCallbacks.set(node, callback);
	getSharedObserver().observe(node);
	return {
		update(next) {
			// Only re-install the callback if the target hasn't
			// already fired (we delete on intersection in the
			// observer callback). Setting on a missing key is a
			// no-op semantically and matches the sticky contract.
			if (sharedCallbacks.has(node)) {
				sharedCallbacks.set(node, next);
			}
		},
		destroy() {
			if (sharedCallbacks.has(node)) {
				sharedCallbacks.delete(node);
				sharedObserver?.unobserve(node);
			}
		},
	};
};
