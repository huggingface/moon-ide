// Folder / file picking that works for remote windows too (ADR 0095):
// a local window uses the native dialog; a remote one browses the
// server's filesystem, since the paths must exist there.

import { open } from '@tauri-apps/plugin-dialog';

import { ipc } from './ipc';
import { formatError, type BrowseResult } from './protocol';
import { remote } from './remote.svelte';

type Mode = 'folder' | 'file';

class PathPickerStore {
	mode = $state<Mode | null>(null);
	listing = $state<BrowseResult | null>(null);
	pathInput = $state('');
	error = $state<string | null>(null);
	loading = $state(false);
	#resolve: ((path: string | null) => void) | null = null;

	async pickFolder(): Promise<string | null> {
		if (remote.target === null) {
			const selected = await open({ directory: true, multiple: false });
			return typeof selected === 'string' ? selected : null;
		}
		return this.#openModal('folder');
	}

	async pickFile(): Promise<string | null> {
		if (remote.target === null) {
			const selected = await open({ directory: false, multiple: false });
			return typeof selected === 'string' ? selected : null;
		}
		return this.#openModal('file');
	}

	async browse(path: string | null) {
		this.loading = true;
		this.error = null;
		try {
			this.listing = await ipc.fs.browse(path);
			this.pathInput = this.listing.path;
		} catch (err) {
			this.error = formatError(err);
		} finally {
			this.loading = false;
		}
	}

	enter(name: string) {
		if (this.listing === null) {
			return;
		}
		void this.browse(join(this.listing.path, name));
	}

	choose(path: string) {
		this.#finish(path);
	}

	cancel() {
		this.#finish(null);
	}

	#openModal(mode: Mode): Promise<string | null> {
		this.#finish(null);
		this.mode = mode;
		this.listing = null;
		this.pathInput = '';
		void this.browse(null);
		return new Promise((resolve) => {
			this.#resolve = resolve;
		});
	}

	#finish(path: string | null) {
		const resolve = this.#resolve;
		this.#resolve = null;
		this.mode = null;
		resolve?.(path);
	}
}

export function join(dir: string, name: string): string {
	return dir.endsWith('/') ? `${dir}${name}` : `${dir}/${name}`;
}

export const pathPicker = new PathPickerStore();
