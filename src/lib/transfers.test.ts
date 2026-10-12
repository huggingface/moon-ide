import { beforeEach, describe, expect, it, vi } from 'vitest';

const ipcMock = vi.hoisted(() => ({
	fs: { readDir: vi.fn() },
	transfer: { upload: vi.fn(), download: vi.fn(), cancel: vi.fn() },
}));
const dialogMock = vi.hoisted(() => ({ confirm: vi.fn(), open: vi.fn(), save: vi.fn() }));
const progressHandlers = vi.hoisted(() => [] as ((payload: unknown) => void)[]);

vi.mock('./ipc', () => ({ ipc: ipcMock }));
vi.mock('@tauri-apps/plugin-dialog', () => dialogMock);
vi.mock('@tauri-apps/api/path', () => ({
	downloadDir: () => Promise.resolve('/home/me/Downloads'),
	join: (...parts: string[]) => Promise.resolve(parts.join('/')),
}));
vi.mock('./transport', () => ({
	subscribe: (_event: string, handler: (payload: unknown) => void) => {
		progressHandlers.push(handler);
		return Promise.resolve(() => {});
	},
}));

const { formatBytes, percent, transfers } = await import('./transfers.svelte');

function entry(name: string, kind: 'file' | 'dir') {
	return { name, path: `/srv/ws/${name}`, kind, size: 1, mtime_ms: null, is_hidden: false };
}

describe('formatBytes', () => {
	it('uses 1000-multiples', () => {
		expect(formatBytes(999)).toBe('999 B');
		expect(formatBytes(1500)).toBe('1.5 kB');
		expect(formatBytes(2_500_000)).toBe('2.5 MB');
		expect(formatBytes(4_200_000_000)).toBe('4.20 GB');
	});
});

describe('percent', () => {
	it('clamps and handles unknown totals', () => {
		expect(percent(50, 200)).toBe(25);
		expect(percent(10, 0)).toBe(0);
		expect(percent(300, 200)).toBe(100);
	});
});

describe('transfers.upload', () => {
	beforeEach(() => {
		vi.clearAllMocks();
		transfers.rows = [];
		ipcMock.transfer.upload.mockResolvedValue('t1');
	});

	it('uploads without asking when nothing clashes', async () => {
		ipcMock.fs.readDir.mockResolvedValue([entry('other.txt', 'file')]);
		await transfers.upload(['/home/me/a.bin'], '/srv/ws/');
		expect(dialogMock.confirm).not.toHaveBeenCalled();
		expect(ipcMock.transfer.upload).toHaveBeenCalledWith(['/home/me/a.bin'], '/srv/ws');
	});

	it('asks once before replacing and respects a no', async () => {
		ipcMock.fs.readDir.mockResolvedValue([entry('a.bin', 'file'), entry('b.bin', 'file')]);
		dialogMock.confirm.mockResolvedValue(false);
		await transfers.upload(['/home/me/a.bin', '/home/me/b.bin'], '/srv/ws');
		expect(dialogMock.confirm).toHaveBeenCalledTimes(1);
		expect(ipcMock.transfer.upload).not.toHaveBeenCalled();
	});

	it('refuses to replace a folder', async () => {
		ipcMock.fs.readDir.mockResolvedValue([entry('a.bin', 'dir')]);
		await transfers.upload(['/home/me/a.bin'], '/srv/ws');
		expect(ipcMock.transfer.upload).not.toHaveBeenCalled();
		expect(transfers.rows[0]?.state).toBe('failed');
	});

	it('tracks progress events and keeps failures', async () => {
		ipcMock.fs.readDir.mockResolvedValue([]);
		await transfers.upload(['/home/me/a.bin'], '/srv/ws');
		const emit = progressHandlers[0];
		expect(emit).toBeDefined();
		emit?.({ id: 't1', name: 'a.bin', bytes: 10, total: 100, state: 'running' });
		expect(transfers.rows).toHaveLength(1);
		expect(transfers.rows[0]?.direction).toBe('upload');
		emit?.({ id: 't1', name: 'a.bin', bytes: 10, total: 100, state: 'failed', error: 'disk full' });
		expect(transfers.rows[0]?.error).toBe('disk full');
		transfers.dismiss('t1');
		expect(transfers.rows).toHaveLength(0);
	});
});
