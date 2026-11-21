// @vitest-environment happy-dom
import { describe, expect, it } from 'vitest';
import { decodePreview, previewDocument } from './htmlPreview';
import { renderMarkdownBlocks, type BlockRenderOptions } from './markdown';

const PREVIEW = '```html preview\n<div style="color: red">Hi</div>\n```';

async function rendered(source: string, options: BlockRenderOptions = { linkify: true }): Promise<string> {
	return (await renderMarkdownBlocks(source, options)).map((block) => block.html).join('');
}

describe('html previews', () => {
	it('an agent reply draws a closed `html preview` fence, with its source', async () => {
		const html = await rendered(`Two options:\n\n${PREVIEW}\n`);
		const slot = new DOMParser().parseFromString(html, 'text/html').querySelector('.md-html-preview');
		expect(decodePreview(slot?.getAttribute('data-html') ?? '')).toBe('<div style="color: red">Hi</div>\n');
		expect(decodePreview(btoa('caf\u00c3\u00a9'))).toBe('caf\u00e9');
		expect(html).toContain('<details class="md-html-source"><summary>Source</summary>');
	});

	it('stays code while the fence streams, as plain html and in user text', async () => {
		expect(await rendered('```html preview\n<div>Hi</div>\n')).not.toContain('md-html-preview');
		expect(await rendered('```html\n<div>Hi</div>\n```')).not.toContain('md-html-preview');
		expect(await rendered(PREVIEW, { linkify: true, breaks: true })).not.toContain('md-html-preview');
		expect(await rendered(PREVIEW, { linkify: false })).not.toContain('md-html-preview');
	});

	it('the frame document blocks every fetch and carries the theme', () => {
		const doc = previewDocument('<p>x</p>', { '--m-fg': '#eee', '--m-bg': '' });
		expect(doc).toContain(`content="default-src 'none'; style-src 'unsafe-inline'; img-src data:; font-src data:"`);
		expect(doc).toContain('--m-fg: #eee;');
		expect(doc).not.toContain('--m-bg:');
		expect(doc.indexOf('Content-Security-Policy')).toBeLessThan(doc.indexOf('<p>x</p>'));
	});
});
