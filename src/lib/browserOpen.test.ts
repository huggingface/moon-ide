import { describe, expect, it, vi } from 'vitest';

vi.mock('./container.svelte', () => ({ container: { state: null } }));
vi.mock('./ipc', () => ({ ipc: {} }));

const { normalizeBrowserUrl } = await import('./browserOpen');

describe('normalizeBrowserUrl', () => {
	it('adds http for local addresses and https otherwise', () => {
		expect(normalizeBrowserUrl('localhost:5173')).toBe('http://localhost:5173/');
		expect(normalizeBrowserUrl(' 127.0.0.1:3000/x ')).toBe('http://127.0.0.1:3000/x');
		expect(normalizeBrowserUrl('example.com/docs')).toBe('https://example.com/docs');
		expect(normalizeBrowserUrl('http://example.com')).toBe('http://example.com/');
	});

	it('rejects empty input and other schemes', () => {
		expect(normalizeBrowserUrl('')).toBeNull();
		expect(normalizeBrowserUrl('file:///etc/passwd')).toBeNull();
	});
});
