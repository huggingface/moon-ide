// Publishes an editor's visible width and its gutter width as CSS
// custom properties on the scroller, for block widgets that must be
// sized against the pane rather than against the content.
//
// A block widget is as wide as `.cm-content`, i.e. as the longest
// line. A card or bar inside it that should fit the *visible* pane
// (and stay put while the code scrolls sideways) needs the pane's
// width in CSS. Container-query units would give it without script,
// but they need `container-type` on the scroller, whose containment
// we would rather not impose on every editor; two measured pixels
// values behave the same in every webview.
//
//   --m-pane-w    the scroller's visible width
//   --m-gutter-w  the width of the gutters pinned at its left edge

import { ViewPlugin, type EditorView, type ViewUpdate } from '@codemirror/view';

class PaneMetrics {
	private readonly observer: ResizeObserver;
	private gutters: Element | null = null;
	private frame = -1;

	constructor(private readonly view: EditorView) {
		// Written a frame later, not inside the observer callback:
		// the widths decide how tall the cards are, hence how tall the
		// scroller being observed is, and resizing an observed element
		// from its own callback is a "ResizeObserver loop" error.
		this.observer = new ResizeObserver(() => this.schedule());
		this.observer.observe(view.scrollDOM);
		this.track();
	}

	update(update: ViewUpdate): void {
		if (update.geometryChanged || this.gutters === null) {
			this.track();
		}
	}

	destroy(): void {
		this.observer.disconnect();
		cancelAnimationFrame(this.frame);
	}

	// The gutters are another plugin's DOM and may not exist yet when
	// this one is built.
	private track(): void {
		const gutters = this.view.scrollDOM.querySelector('.cm-gutters');
		if (gutters === this.gutters) {
			return;
		}
		if (this.gutters !== null) {
			this.observer.unobserve(this.gutters);
		}
		if (gutters !== null) {
			this.observer.observe(gutters);
		}
		this.gutters = gutters;
	}

	private schedule(): void {
		if (this.frame !== -1) {
			return;
		}
		this.frame = requestAnimationFrame(() => {
			this.frame = -1;
			this.publish();
		});
	}

	private publish(): void {
		const scroller = this.view.scrollDOM;
		// Not laid out (detached, hidden): keep what was there, or the
		// CSS fallbacks, rather than publish a zero.
		if (scroller.clientWidth === 0) {
			return;
		}
		const gutters = this.gutters instanceof HTMLElement ? this.gutters.offsetWidth : 0;
		scroller.style.setProperty('--m-pane-w', `${scroller.clientWidth}px`);
		scroller.style.setProperty('--m-gutter-w', `${gutters}px`);
	}
}

export const paneMetrics = ViewPlugin.fromClass(PaneMetrics);
