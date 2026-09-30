// @vitest-environment happy-dom
import { beforeAll, describe, expect, it } from 'vitest';

import BRIDGE from '../../crates/moon-container/src/preview_bridge.js?raw';
import { toOriginalUrl } from './browserBridge';

// The page bridge (ADR 0088) is plain JS injected by the preview
// proxy; exercise it here against a DOM with a fake IDE parent.

type Posted = { kind: string; id?: number; ok?: boolean; value?: unknown; error?: string; href?: string };

/** `value[key]` of a bridge result, as a string. */
function field(res: Posted, key: string): string {
	const value: unknown = res.value;
	if (typeof value !== 'object' || value === null || !(key in value)) {
		return '';
	}
	const entry: unknown = Reflect.get(value, key);
	return typeof entry === 'string' ? entry : JSON.stringify(entry);
}
const posted: Posted[] = [];
const parent = {
	postMessage: (msg: Posted) => {
		posted.push(msg);
	},
};

let nextId = 1;
async function request(op: string, args: Record<string, unknown> = {}): Promise<Posted> {
	const id = nextId++;
	const event = new MessageEvent('message', { data: { moonIdeBridge: 1, kind: 'request', id, op, args } });
	Object.defineProperty(event, 'source', { value: parent });
	window.dispatchEvent(event);
	for (let i = 0; i < 50; i++) {
		const hit = posted.find((m) => m.kind === 'result' && m.id === id);
		if (hit) {
			return hit;
		}
		await new Promise((done) => setTimeout(done, 10));
	}
	throw new Error(`no answer to ${op}`);
}

beforeAll(() => {
	Object.defineProperty(window, 'parent', { value: parent, configurable: true });
	Object.defineProperty(window, 'top', { value: parent, configurable: true });
	document.body.innerHTML = `
		<h1>Models</h1>
		<form id="f"><input name="q" placeholder="Search"><button type="submit">Go</button></form>
		<p hidden>secret</p>
		<button id="b">Like</button>`;
	document.querySelector('#b')?.addEventListener('click', () => {
		document.title = 'clicked';
	});
	// oxlint-disable-next-line no-eval -- loading the bridge the way the page would.
	(0, eval)(BRIDGE);
});

describe('page bridge', () => {
	it('says hello with the page location', () => {
		expect(posted[0]?.kind).toBe('hello');
	});

	it('snapshots visible content with refs', async () => {
		const res = await request('snapshot');
		expect(res.ok).toBe(true);
		const text = field(res, 'snapshot');
		expect(text).toContain('# Models');
		expect(text).toMatch(/\[e\d+\] input\[text\] name=q/);
		expect(text).toMatch(/\[e\d+\] button "Like"/);
		expect(text).not.toContain('secret');
	});

	it('types and clicks by ref / selector', async () => {
		const snap = await request('snapshot');
		const ref = /\[(e\d+)\] input/.exec(field(snap, 'snapshot'))?.[1];
		await request('type', { ref, text: 'llama' });
		expect(document.querySelector('input')?.value).toBe('llama');
		await request('click', { selector: '#b' });
		expect(document.title).toBe('clicked');
	});

	it('evaluates, captures console, and reports failures', async () => {
		const res = await request('eval', { expression: 'Promise.resolve(6 * 7)' });
		expect(field(res, 'value')).toBe('42');
		// oxlint-disable-next-line no-console -- the bridge's console capture is what's under test.
		console.warn('careful', { a: 1 });
		const logs = await request('console');
		expect(field(logs, 'entries')).toContain('"level":"warn","text":"careful {\\"a\\":1}"');
		const bad = await request('click', { ref: 'e999' });
		expect(bad.ok).toBe(false);
		expect(bad.error).toContain('unknown ref');
	});
});

describe('toOriginalUrl', () => {
	it('maps the proxy origin back and leaves others alone', () => {
		expect(toOriginalUrl('http://127.0.0.1:4100/a?b#c', 'http://127.0.0.1:4100', 'http://localhost:5173')).toBe(
			'http://localhost:5173/a?b#c',
		);
		expect(toOriginalUrl('https://hf.co/x', 'http://127.0.0.1:4100', 'http://localhost:5173')).toBe('https://hf.co/x');
	});
});
