// Rendering text written on GitHub (descriptions, comments) into an
// element, for the pull-request and issue views.

import { openUrl } from '@tauri-apps/plugin-opener';
import { getCachedMarkdown, renderMarkdown } from './markdown';

/** Svelte action: fill `el` with `source` rendered in GitHub's
 *  dialect. Paints synchronously on a cache hit; otherwise shows the
 *  source text until the render resolves. */
export function githubMarkdown(el: HTMLElement, source: string): { update: (source: string) => void } {
	let current = '';
	const render = (text: string) => {
		current = text;
		const cached = getCachedMarkdown(text, { github: true });
		if (cached !== undefined) {
			el.innerHTML = cached;
			return;
		}
		el.textContent = text;
		void renderMarkdown(text, { github: true }).then((html) => {
			if (el.isConnected && current === text) {
				el.innerHTML = html;
			}
		});
	};
	render(source);
	return { update: render };
}

/** Click handler for a container of rendered markdown: links open in
 *  the system browser instead of navigating the webview. */
export function openMarkdownLinks(event: MouseEvent): void {
	const anchor = event.target instanceof Element ? event.target.closest('a') : null;
	if (anchor === null) {
		return;
	}
	event.preventDefault();
	const href = anchor.getAttribute('href') ?? '';
	if (/^https?:\/\//.test(href)) {
		void openUrl(href);
	}
}
