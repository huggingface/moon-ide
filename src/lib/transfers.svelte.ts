// File transfers between this desktop and the machine running the
// workspace (Phase 16.6, ADR 0095). Uploads land in a folder of the
// workspace (the server's, for a remote window); downloads land on
// this desktop. The backend streams and reports progress on
// `transfer:progress`; this store keeps the toast stack's rows.

import { confirm, open, save } from '@tauri-apps/plugin-dialog';
import { downloadDir, join } from '@tauri-apps/api/path';

import { ipc } from './ipc';
import { formatError } from './protocol';
import { subscribe } from './transport';

/** Must match `PROGRESS_EVENT` in `src-tauri/src/transfer.rs`. */
const PROGRESS_EVENT = 'transfer:progress';
const DONE_LINGER_MS = 3000;

export type TransferState = 'running' | 'done' | 'failed' | 'cancelled';

export type TransferProgress = {
	id: string;
	name: string;
	bytes: number;
	total: number;
	state: TransferState;
	error?: string;
};

export type TransferRow = TransferProgress & { direction: 'upload' | 'download' };

/** 1000-multiples, per the house rules. */
export function formatBytes(n: number): string {
	if (n < 1000) {
		return `${n} B`;
	}
	if (n < 1000 ** 2) {
		return `${(n / 1000).toFixed(1)} kB`;
	}
	if (n < 1000 ** 3) {
		return `${(n / 1000 ** 2).toFixed(1)} MB`;
	}
	return `${(n / 1000 ** 3).toFixed(2)} GB`;
}

export function percent(bytes: number, total: number): number {
	if (total <= 0) {
		return 0;
	}
	return Math.min(100, Math.floor((bytes / total) * 100));
}

function basename(path: string): string {
	const trimmed = path.replace(/\/+$/, '');
	return trimmed.slice(trimmed.lastIndexOf('/') + 1);
}

class TransfersStore {
	rows = $state<TransferRow[]>([]);

	#listening: Promise<unknown> | null = null;
	#directions = new Map<string, TransferRow['direction']>();

	#listen(): Promise<unknown> {
		this.#listening ??= subscribe(PROGRESS_EVENT, (payload: TransferProgress) => {
			this.#apply(payload);
		});
		return this.#listening;
	}

	#apply(progress: TransferProgress) {
		const direction = this.#directions.get(progress.id) ?? 'upload';
		const row: TransferRow = { ...progress, direction };
		const index = this.rows.findIndex((r) => r.id === progress.id);
		if (index < 0) {
			this.rows.push(row);
		} else {
			this.rows[index] = row;
		}
		if (progress.state === 'done' || progress.state === 'cancelled') {
			this.#directions.delete(progress.id);
			setTimeout(() => this.dismiss(progress.id), progress.state === 'done' ? DONE_LINGER_MS : 0);
		}
	}

	dismiss(id: string) {
		this.rows = this.rows.filter((r) => r.id !== id);
	}

	cancel(id: string) {
		void ipc.transfer.cancel(id);
	}

	/** Pick desktop files with the native dialog and upload them into `targetDir`. */
	async pickAndUpload(targetDir: string) {
		const picked = await open({ multiple: true, directory: false, title: 'Upload files' });
		if (!picked || picked.length === 0) {
			return;
		}
		await this.upload(picked, targetDir);
	}

	/**
	 * Upload desktop `sources` into `targetDir` (absolute, on the
	 * workspace's machine), asking once before replacing anything.
	 */
	async upload(sources: string[], targetDir: string) {
		const dir = targetDir.replace(/\/+$/, '') || '/';
		let existing: Map<string, boolean>;
		try {
			const entries = await ipc.fs.readDir(dir);
			existing = new Map(entries.map((e) => [e.name, e.kind === 'dir']));
		} catch (err) {
			this.#fail('upload', sources, `Cannot read ${dir}: ${formatError(err)}`);
			return;
		}
		const names = sources.map(basename);
		const folderClash = names.find((name) => existing.get(name) === true);
		if (folderClash !== undefined) {
			this.#fail('upload', sources, `${folderClash} is a folder in ${dir}`);
			return;
		}
		const clashes = names.filter((name) => existing.has(name));
		if (clashes.length > 0) {
			const list = clashes.length === 1 ? clashes[0] : `${clashes.length} files (${clashes.join(', ')})`;
			const ok = await confirm(`Replace ${list} in ${dir}?`, {
				title: 'Replace files',
				kind: 'warning',
				okLabel: 'Replace',
				cancelLabel: 'Cancel',
			});
			if (!ok) {
				return;
			}
		}
		await this.#start('upload', sources, () => ipc.transfer.upload(sources, dir));
	}

	/** Ask where to save `source` (absolute, on the workspace's machine) and download it. */
	async download(source: string) {
		const name = basename(source);
		const defaultPath = await downloadDir()
			.then((dir) => join(dir, name))
			.catch(() => name);
		const destination = await save({ title: 'Download to this computer', defaultPath });
		if (!destination) {
			return;
		}
		await this.#start('download', [source], () => ipc.transfer.download(source, destination));
	}

	async #start(direction: TransferRow['direction'], sources: string[], begin: () => Promise<string>) {
		await this.#listen();
		try {
			const id = await begin();
			this.#directions.set(id, direction);
			const row = this.rows.find((r) => r.id === id);
			if (row) {
				row.direction = direction;
			}
		} catch (err) {
			this.#fail(direction, sources, formatError(err));
		}
	}

	#fail(direction: TransferRow['direction'], sources: string[], error: string) {
		const name = sources.length === 1 ? basename(sources[0] ?? '') : `${sources.length} files`;
		this.rows.push({
			id: `local-${crypto.randomUUID()}`,
			name,
			bytes: 0,
			total: 0,
			state: 'failed',
			error,
			direction,
		});
	}
}

export const transfers = new TransfersStore();
