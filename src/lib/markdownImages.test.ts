// @vitest-environment happy-dom
import { describe, expect, it } from 'vitest';

import { resolveCoderImages, resolveMarkdownImages } from './markdown';

// `resolveMarkdownImages` runs on sanitised HTML, so the fixtures
// here are plain markdown-it-shaped output. The `toUrl` resolver is
// injected — the production default goes through Tauri IPC, which
// doesn't exist under vitest.
const toUrl = async (workspacePath: string) => `asset://localhost/root/${workspacePath}`;

describe('resolveMarkdownImages', () => {
	it('resolves a relative src against the markdown file directory', async () => {
		const html = '<p><img src="img/pic.png" alt="a"></p>';
		const out = await resolveMarkdownImages(html, 'docs/readme.md', toUrl);
		expect(out).toContain('src="asset://localhost/root/docs/img/pic.png"');
		expect(out).toContain('alt="a"');
	});

	it('resolves ./ and ../ segments', async () => {
		const html = '<p><img src="./a.png"><img src="../shared/b.png"></p>';
		const out = await resolveMarkdownImages(html, 'docs/sub/readme.md', toUrl);
		expect(out).toContain('src="asset://localhost/root/docs/sub/a.png"');
		expect(out).toContain('src="asset://localhost/root/docs/shared/b.png"');
	});

	it('treats a leading slash as workspace-root-absolute', async () => {
		const html = '<p><img src="/assets/logo.svg"></p>';
		const out = await resolveMarkdownImages(html, 'docs/readme.md', toUrl);
		expect(out).toContain('src="asset://localhost/root/assets/logo.svg"');
	});

	it('returns the input unchanged when there is nothing to rewrite', async () => {
		const absolute =
			'<p><img src="https://example.com/x.png"><img src="data:image/png;base64,AAAA"><img src="//cdn.example.com/y.png"></p>';
		await expect(resolveMarkdownImages(absolute, 'docs/readme.md', toUrl)).resolves.toBe(absolute);
		const noImages = '<p>hello</p>';
		await expect(resolveMarkdownImages(noImages, 'docs/readme.md', toUrl)).resolves.toBe(noImages);
	});

	it('keeps the original src when the link escapes the workspace or the resolver fails', async () => {
		const html = '<p><img src="../../etc/passwd"><img src="broken.png"></p>';
		const out = await resolveMarkdownImages(html, 'docs/readme.md', async () => null);
		expect(out).toBe(html);
	});
});

describe('resolveCoderImages', () => {
	const resolve = async (path: string) => (path.startsWith('/') ? null : `asset://localhost/root/${path}`);

	const read = async (path: string) => (path.startsWith('/tmp/') ? `data:image/png;base64,${btoa(path)}` : null);

	it('resolves workspace, outside, and root-relative paths, and skips URLs', async () => {
		const root = document.createElement('div');
		root.innerHTML =
			'<img src="./out/shot.png" alt="a"><img src="/charts/x.png" alt="b"><img src="https://e.com/y.png" alt="c"><img src="/tmp/s.png" alt="d">';
		await resolveCoderImages(root, resolve, read);
		const [a, b, c, d] = Array.from(root.querySelectorAll('img'));
		expect(d?.getAttribute('src')).toBe(`data:image/png;base64,${btoa('/tmp/s.png')}`);
		expect(d?.dataset.workspacePath).toBeUndefined();
		expect(a?.getAttribute('src')).toBe('asset://localhost/root/out/shot.png');
		expect(a?.dataset.workspacePath).toBe('out/shot.png');
		expect(b?.getAttribute('src')).toBe('asset://localhost/root/charts/x.png');
		expect(b?.dataset.workspacePath).toBe('charts/x.png');
		expect(c?.getAttribute('src')).toBe('https://e.com/y.png');
		expect(c?.dataset.workspacePath).toBeUndefined();
	});
});
