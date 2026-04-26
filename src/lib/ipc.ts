import { invoke } from '@tauri-apps/api/core';
import type {
	AppState,
	ContentSearchOptions,
	ContentSearchResult,
	DirEntry,
	FileSearchOptions,
	FileSearchResult,
	ReadFileResult,
	StatResult,
	Workspace,
	WriteFileResult,
} from './protocol';

// Single auditable surface for all Tauri commands. Components MUST go through this.
export const ipc = {
	workspace: {
		openLocal: (path: string) => invoke<Workspace>('workspace_open_local', { path }),
		active: () => invoke<Workspace | null>('workspace_active'),
		list: () => invoke<Workspace[]>('workspace_list'),
	},
	fs: {
		readDir: (path: string) => invoke<DirEntry[]>('fs_read_dir', { path }),
		readFile: (path: string) => invoke<ReadFileResult>('fs_read_file', { path }),
		writeFile: (path: string, text: string) => invoke<WriteFileResult>('fs_write_file', { path, text }),
		stat: (path: string) => invoke<StatResult>('fs_stat', { path }),
		absolutePath: (path: string) => invoke<string>('fs_absolute_path', { path }),
	},
	search: {
		files: (options: FileSearchOptions) => invoke<FileSearchResult[]>('search_files', { options }),
		content: (options: ContentSearchOptions) => invoke<ContentSearchResult>('search_content', { options }),
	},
	appState: {
		load: () => invoke<AppState>('app_state_load'),
		save: (appState: AppState) => invoke<void>('app_state_save', { appState }),
	},
} as const;
