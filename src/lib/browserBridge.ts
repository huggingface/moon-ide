// Relay between the coder's `browser_page` requests and the page
// bridge running inside each browser tab's iframe (ADR 0088).
//
// Backend -> `browser:page_request` event -> the tab's iframe via
// postMessage -> the injected bridge answers -> `browser_page_respond`.
// Every tab's iframe stays mounted (ADR 0105), so a request only
// waits in a queue until the page's bridge says hello (first load,
// reload, navigation).

import { browserCapture } from './browserFrames.svelte';
import { subscribe } from './transport';
import { ipc } from './ipc';
import type { BrowserPageRequest } from './protocol';

const MARK = 'moonIdeBridge';
// The backend gives up after ~20 s; don't hold requests longer.
const QUEUE_TTL_MS = 30_000;

type Frame = {
	/** The tab's `<iframe>`, measured for screenshots. */
	element: HTMLIFrameElement;
	target: Window;
	/** `http://127.0.0.1:<proxy>` the iframe actually loads. */
	proxiedOrigin: string;
	/** Origin of the URL the tab was asked to show. */
	originalOrigin: string;
	ready: boolean;
};

const frames = new Map<number, Frame>();
const queued = new Map<number, { request: BrowserPageRequest; at: number }[]>();

/** Map a proxied page URL back onto the origin the user/agent asked
 *  for, so reports read `http://localhost:5173/x`, not the proxy. */
export function toOriginalUrl(href: string, proxiedOrigin: string, originalOrigin: string): string {
	try {
		const parsed = new URL(href);
		if (parsed.origin !== proxiedOrigin) {
			return href;
		}
		return originalOrigin + parsed.pathname + parsed.search + parsed.hash;
	} catch {
		return href;
	}
}

function send(frame: Frame, request: BrowserPageRequest) {
	if (request.op === 'screenshot') {
		void capture(frame, request.tab_id, request.request_id);
		return;
	}
	frame.target.postMessage(
		{ [MARK]: 1, kind: 'request', id: request.request_id, op: request.op, args: request.args },
		'*',
	);
}

/** Paint settles within a couple of frames; a window on another
 *  desktop or minimized gets no animation frames, so cap the wait. */
function settle(): Promise<unknown> {
	return Promise.race([
		new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve))),
		new Promise((resolve) => setTimeout(resolve, 200)),
	]);
}

/** Screenshots are taken by the IDE, not the page: a native snapshot
 *  of the window's webview cropped to the iframe (ADR 0103). A tab
 *  that isn't on screen is raised on top for the capture and dropped
 *  back right after — a brief flash instead of a tab switch. */
async function capture(frame: Frame, tabId: number, requestId: number): Promise<void> {
	browserCapture.tabId = tabId;
	let shot: { png: string; width: number; height: number };
	try {
		await settle();
		const rect = frame.element.getBoundingClientRect();
		const png = await ipc.browser.capture(rect.left, rect.top, rect.width, rect.height, window.innerWidth);
		shot = { png, width: Math.round(rect.width), height: Math.round(rect.height) };
	} catch (err) {
		await ipc.browser.respond(requestId, null, err instanceof Error ? err.message : String(err));
		return;
	} finally {
		if (browserCapture.tabId === tabId) {
			browserCapture.tabId = null;
		}
	}
	await ipc.browser.respond(requestId, { png_base64: shot.png, width: shot.width, height: shot.height }, null);
}

function flush(tabId: number, frame: Frame) {
	const pending = queued.get(tabId) ?? [];
	queued.delete(tabId);
	const now = Date.now();
	for (const { request, at } of pending) {
		if (now - at < QUEUE_TTL_MS) {
			send(frame, request);
		}
	}
}

/** Register a mounted iframe for `tabId`; returns its teardown. The
 *  frame counts as ready once its page bridge says hello. */
export function registerFrame(tabId: number, frame: Omit<Frame, 'ready'>): () => void {
	const entry: Frame = { ...frame, ready: false };
	frames.set(tabId, entry);
	return () => {
		if (frames.get(tabId) === entry) {
			frames.delete(tabId);
		}
	};
}

/** Handle one postMessage from tab `tabId`'s iframe. `currentUrl` is
 *  the tab's registry URL, to skip no-op location reports. */
export function handleFrameMessage(tabId: number, data: unknown, currentUrl: string) {
	const frame = frames.get(tabId);
	if (!frame || typeof data !== 'object' || data === null) {
		return;
	}
	const msg: Record<string, unknown> = { ...data };
	if (msg[MARK] !== 1) {
		return;
	}
	if (msg.kind === 'hello' || msg.kind === 'location') {
		if (typeof msg.href === 'string') {
			const url = toOriginalUrl(msg.href, frame.proxiedOrigin, frame.originalOrigin);
			if (url !== currentUrl) {
				void ipc.browser.location(tabId, url);
			}
		}
		if (msg.kind === 'hello') {
			frame.ready = true;
			flush(tabId, frame);
		}
		return;
	}
	if (msg.kind !== 'result' || typeof msg.id !== 'number') {
		return;
	}
	if (msg.ok !== true) {
		void ipc.browser.respond(msg.id, null, typeof msg.error === 'string' ? msg.error : 'page action failed');
		return;
	}
	let value = msg.value;
	if (typeof value === 'object' && value !== null && 'href' in value && typeof value.href === 'string') {
		value = { ...value, href: toOriginalUrl(value.href, frame.proxiedOrigin, frame.originalOrigin) };
	}
	void ipc.browser.respond(msg.id, value ?? null, null);
}

let wired = false;

/** Subscribe to `browser:page_request` once per webview. */
export async function wireBrowserBridge(): Promise<void> {
	if (wired) {
		return;
	}
	wired = true;
	await subscribe('browser:page_request', (payload: BrowserPageRequest) => {
		const frame = frames.get(payload.tab_id);
		if (frame?.ready) {
			send(frame, payload);
			return;
		}
		const list = queued.get(payload.tab_id) ?? [];
		list.push({ request: payload, at: Date.now() });
		queued.set(payload.tab_id, list);
	});
}
