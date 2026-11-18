// Images attached to pull requests and issues of a private repository
// (`github.com/user-attachments/assets/<id>`). GitHub serves them only
// to a browser signed in to github.com, so the webview gets a 404. The
// core trades each address for a signed URL that does load; those last
// five minutes, hence the short memory here.

import { ipc } from './ipc';

/** How long a signed URL is handed out again. GitHub's are good for
 *  300 s; an image only has to start loading within that. */
const FRESH_MS = 240_000;
/** Addresses asked for within this window go out as one request. */
const BATCH_MS = 25;

/** Whether `url` is an attachment only a signed-in browser can load. */
export function isPrivateAsset(url: string): boolean {
	return /^https:\/\/github\.com\/(?:user-attachments\/assets|[^/]+\/[^/]+\/assets\/\d+)\/[0-9a-f-]{8,}$/i.test(url);
}

/** `owner/name` of a repository page URL on github.com. */
export function repoOf(repoUrl: string | null): string | null {
	const match = repoUrl === null ? null : /^https:\/\/github\.com\/([^/]+\/[^/]+?)\/?$/.exec(repoUrl);
	return match?.[1] ?? null;
}

type Signed = { url: string | null; at: number };
const signed = new Map<string, Signed>();
type Batch = { urls: Set<string>; done: Promise<void> };
const batches = new Map<string, Batch>();

function fresh(url: string, now: number): Signed | undefined {
	const known = signed.get(url);
	return known !== undefined && now - known.at < FRESH_MS ? known : undefined;
}

function request(repo: string, url: string): Promise<void> {
	let batch = batches.get(repo);
	if (batch === undefined) {
		const urls = new Set<string>();
		const done = new Promise<void>((resolve) => {
			setTimeout(() => {
				batches.delete(repo);
				const asked = [...urls];
				ipc.inbox
					.assetUrls(repo, asked)
					.then((answers) => {
						const at = Date.now();
						asked.forEach((one, index) => signed.set(one, { url: answers[index] ?? null, at }));
					})
					// Left unsigned: the image keeps its alt text.
					.catch(() => undefined)
					.finally(resolve);
			}, BATCH_MS);
		});
		batch = { urls, done };
		batches.set(repo, batch);
	}
	batch.urls.add(url);
	return batch.done;
}

/**
 * Make the private attachments among `el`'s images loadable. Until an
 * image has its signed URL it has no `src` at all, so the webview
 * never asks github.com for something it will be refused.
 */
export function loadPrivateAssets(el: HTMLElement, repoUrl: string | null): void {
	const repo = repoOf(repoUrl);
	if (repo === null) {
		return;
	}
	const now = Date.now();
	for (const img of el.querySelectorAll<HTMLImageElement>('img[src]')) {
		const original = img.getAttribute('src') ?? '';
		if (!isPrivateAsset(original)) {
			continue;
		}
		img.removeAttribute('src');
		const show = () => {
			const url = signed.get(original)?.url ?? null;
			if (url !== null && img.isConnected) {
				img.src = url;
			}
		};
		if (fresh(original, now) !== undefined) {
			show();
			continue;
		}
		void request(repo, original).then(show);
	}
}
