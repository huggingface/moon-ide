// Shared state between browser-tab views and the frame layer that owns
// their iframes (ADR 0105). Iframes live in one app-level layer so a
// tab's page survives tab and folder switches; a `BrowserView` only
// publishes the slot its tab's frame should cover.

import { SvelteMap } from 'svelte/reactivity';

export type BrowserFrameStatus = { src: string; originalOrigin: string } | { error: string };

/** Slot element a visible `BrowserView` reserves for each tab id. */
export const browserSlots = new SvelteMap<number, HTMLElement>();

/** Resolution result per tab id, for the view's error / "open
 *  externally" affordances. */
export const browserFrameStatus = new SvelteMap<number, BrowserFrameStatus>();

/** Tab whose hidden frame is raised on top for a screenshot; the
 *  snapshot only sees what is painted. */
export const browserCapture = $state<{ tabId: number | null }>({ tabId: null });

/** Reserve `node` as tab `tabId`'s slot; returns its release. */
export function registerSlot(tabId: number, node: HTMLElement): () => void {
	browserSlots.set(tabId, node);
	return () => {
		if (browserSlots.get(tabId) === node) {
			browserSlots.delete(tabId);
		}
	};
}
