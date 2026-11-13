import { container } from './container.svelte';
import { ipc } from './ipc';

const LOCAL_HOSTS = new Set(['localhost', '127.0.0.1', '0.0.0.0', '[::1]']);

/** Turn what the user typed into an http(s) URL: bare local
 *  addresses (`localhost:5173`) get `http://`, other bare hosts
 *  `https://`. `null` when it can't be one. */
export function normalizeBrowserUrl(raw: string): string | null {
	const text = raw.trim();
	if (text === '') {
		return null;
	}
	const withScheme = /^[a-z][a-z0-9+.-]*:\/\//i.test(text)
		? text
		: `${LOCAL_HOSTS.has(text.split(/[:/]/)[0] ?? '') ? 'http' : 'https'}://${text}`;
	let url: URL;
	try {
		url = new URL(withScheme);
	} catch {
		return null;
	}
	if (url.protocol !== 'http:' && url.protocol !== 'https:') {
		return null;
	}
	return url.toString();
}

/** Open `url` in an IDE browser tab (reusing one already on it). Local
 *  addresses dial from the workspace container when it's running,
 *  where the team's dev servers live. */
export async function openInIdeBrowser(url: string): Promise<void> {
	const local = LOCAL_HOSTS.has(new URL(url).hostname);
	await ipc.browser.open(url, local && container.state === 'running', true);
}
