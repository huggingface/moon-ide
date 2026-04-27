import { invoke } from '@tauri-apps/api/core';
import type {
	AppState,
	ContentSearchOptions,
	ContentSearchResult,
	DirEntry,
	EditorConfig,
	FileSearchOptions,
	FileSearchResult,
	ReadFileResult,
	SlackBotProfile,
	SlackIdentity,
	SlackStatus,
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
		trash: (path: string) => invoke<void>('fs_trash', { path }),
		delete: (path: string) => invoke<void>('fs_delete', { path }),
	},
	search: {
		files: (options: FileSearchOptions) => invoke<FileSearchResult[]>('search_files', { options }),
		content: (options: ContentSearchOptions) => invoke<ContentSearchResult>('search_content', { options }),
	},
	appState: {
		load: () => invoke<AppState>('app_state_load'),
		save: (appState: AppState) => invoke<void>('app_state_save', { appState }),
	},
	editorconfig: {
		forPath: (path: string) => invoke<EditorConfig>('editorconfig_for_path', { path }),
	},
	slack: {
		setToken: (token: string) => invoke<SlackIdentity>('slack_set_token', { token }),
		status: () => invoke<SlackStatus>('slack_status'),
		clearToken: () => invoke<void>('slack_clear_token'),
		listDmBots: () => invoke<SlackBotProfile[]>('slack_list_dm_bots'),
		selectBot: (profile: SlackBotProfile) => invoke<void>('slack_select_bot', { profile }),
		clearBot: () => invoke<void>('slack_clear_bot'),
		getActiveBot: () => invoke<SlackBotProfile | null>('slack_get_active_bot'),
	},
} as const;
