// Synthetic path scheme for in-IDE browser tabs (ADR 0088). Each tab
// is keyed on `browser://<id>`, `id` being the backend registry's; the
// URL it shows lives in `browserTabs` (it changes on navigation, the
// key doesn't).
// Same can't-collide-with-real-paths trick as `commit://` — gated
// everywhere through `isSyntheticBufferPath`.

export function browserPath(id: number): string {
	return `browser://${id}`;
}

export function isBrowserPath(path: string): boolean {
	return path.startsWith('browser://');
}

export function browserIdFromPath(path: string): number | null {
	if (!isBrowserPath(path)) {
		return null;
	}
	const id = Number(path.slice('browser://'.length));
	return Number.isInteger(id) && id > 0 ? id : null;
}

// Tab label for a URL: `host:port` plus the path when it's not `/`.
export function browserTabName(url: string): string {
	try {
		const parsed = new URL(url);
		const path = parsed.pathname === '/' ? '' : parsed.pathname;
		return `${parsed.host}${path}`;
	} catch {
		return url;
	}
}
