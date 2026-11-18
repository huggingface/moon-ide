// Rendering text written on GitHub (descriptions, comments) into an
// element, for the pull-request and issue views.

import { openUrl } from '@tauri-apps/plugin-opener';
import { loadPrivateAssets } from './githubAssets';
import { inbox } from './inbox.svelte';
import { getCachedMarkdown, renderMarkdown } from './markdown';

/** What to render: the text, and the repository it was written in
 *  (`https://github.com/<owner>/<name>`), which is where its `#123`
 *  references point. */
export type GithubText = { text: string; repoUrl: string | null };

/** The repository page behind a pull request's or an issue's URL. */
export function repoUrlOf(url: string): string | null {
	const match = /^(https:\/\/[^/]+\/[^/]+\/[^/]+)\/(?:pull|issues)\/\d+/.exec(url);
	return match?.[1] ?? null;
}

/** Mark the mentions of the signed-in user, so being asked something
 *  stands out in a long thread. */
function markOwnMentions(el: HTMLElement): void {
	const viewer = inbox.snapshot?.viewer.toLowerCase() ?? '';
	if (viewer === '') {
		return;
	}
	for (const mention of el.querySelectorAll('a.md-mention')) {
		if (mention.textContent?.toLowerCase() === `@${viewer}`) {
			mention.classList.add('md-mention-me');
		}
	}
}

/** Svelte action: fill `el` with `source` rendered in GitHub's
 *  dialect. Paints synchronously on a cache hit; otherwise shows the
 *  source text until the render resolves. */
export function githubMarkdown(el: HTMLElement, source: GithubText): { update: (source: GithubText) => void } {
	let current: GithubText | null = null;
	const paint = (html: string, repoUrl: string | null) => {
		el.innerHTML = html;
		markOwnMentions(el);
		loadPrivateAssets(el, repoUrl);
	};
	const render = (next: GithubText) => {
		// Repainting would close a `<details>` the reader opened.
		if (current !== null && current.text === next.text && current.repoUrl === next.repoUrl) {
			return;
		}
		current = next;
		const options = { github: true, repoUrl: next.repoUrl };
		const cached = getCachedMarkdown(next.text, options);
		if (cached !== undefined) {
			paint(cached, next.repoUrl);
			return;
		}
		el.textContent = next.text;
		void renderMarkdown(next.text, options).then((html) => {
			if (el.isConnected && current === next) {
				paint(html, next.repoUrl);
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
