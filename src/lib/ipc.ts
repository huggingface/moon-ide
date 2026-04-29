import { invoke } from '@tauri-apps/api/core';
import type {
	AppState,
	ContainerStatus,
	ContentSearchOptions,
	ContentSearchResult,
	DirEntry,
	EditorConfig,
	FileSearchOptions,
	FileSearchResult,
	ProjectComposeStatus,
	ReadFileResult,
	SlackBotProfile,
	SlackIdentity,
	SlackMessage,
	SlackSession,
	SlackStatus,
	SlackUserSummary,
	StatResult,
	Workspace,
	WriteFileResult,
} from './protocol';

// Single auditable surface for all Tauri commands. Components MUST go through this.
export const ipc = {
	workspace: {
		openLocal: (path: string) => invoke<Workspace>('workspace_open_local', { path }),
		removeFolder: (path: string) => invoke<Workspace>('workspace_remove_folder', { path }),
		setActiveFolder: (path: string) => invoke<Workspace>('workspace_set_active_folder', { path }),
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
	container: {
		status: () => invoke<ContainerStatus>('container_status'),
		setup: () => invoke<ContainerStatus>('container_setup'),
		pause: () => invoke<ContainerStatus>('container_pause'),
		resume: () => invoke<ContainerStatus>('container_resume'),
		rebuild: () => invoke<ContainerStatus>('container_rebuild'),
		teardown: () => invoke<ContainerStatus>('container_teardown'),
		applyBoundFolders: () => invoke<ContainerStatus>('container_apply_bound_folders'),
		renderCompose: () => invoke<string>('container_render_compose'),
	},
	projectCompose: {
		status: (folderPath: string) => invoke<ProjectComposeStatus>('project_compose_status', { folderPath }),
		up: (folderPath: string) => invoke<ProjectComposeStatus>('project_compose_up', { folderPath }),
		pause: (folderPath: string) => invoke<ProjectComposeStatus>('project_compose_pause', { folderPath }),
		resume: (folderPath: string) => invoke<ProjectComposeStatus>('project_compose_resume', { folderPath }),
		rebuild: (folderPath: string) => invoke<ProjectComposeStatus>('project_compose_rebuild', { folderPath }),
		stop: (folderPath: string) => invoke<ProjectComposeStatus>('project_compose_stop', { folderPath }),
		down: (folderPath: string) => invoke<ProjectComposeStatus>('project_compose_down', { folderPath }),
		serviceStart: (folderPath: string, service: string) =>
			invoke<ProjectComposeStatus>('project_compose_service_start', { folderPath, service }),
		serviceStop: (folderPath: string, service: string) =>
			invoke<ProjectComposeStatus>('project_compose_service_stop', { folderPath, service }),
		serviceRestart: (folderPath: string, service: string) =>
			invoke<ProjectComposeStatus>('project_compose_service_restart', { folderPath, service }),
	},
	composeLogs: {
		open: (folderPath: string, service: string) => invoke<string>('compose_logs_open', { folderPath, service }),
		close: (streamId: string) => invoke<void>('compose_logs_close', { streamId }),
	},
	slack: {
		setToken: (token: string) => invoke<SlackIdentity>('slack_set_token', { token }),
		status: () => invoke<SlackStatus>('slack_status'),
		clearToken: () => invoke<void>('slack_clear_token'),
		listDmBots: () => invoke<SlackBotProfile[]>('slack_list_dm_bots'),
		selectBot: (profile: SlackBotProfile) => invoke<void>('slack_select_bot', { profile }),
		clearBot: () => invoke<void>('slack_clear_bot'),
		getActiveBot: () => invoke<SlackBotProfile | null>('slack_get_active_bot'),
		setPanelVisible: (visible: boolean) => invoke<void>('slack_set_panel_visible', { visible }),
		setWindowFocused: (focused: boolean) => invoke<void>('slack_set_window_focused', { focused }),
		listSessions: (channel: string) => invoke<SlackSession[]>('slack_list_sessions', { channel }),
		getThread: (channel: string, threadTs: string) => invoke<SlackMessage[]>('slack_get_thread', { channel, threadTs }),
		setActiveThread: (threadTs: string | null) => invoke<void>('slack_set_active_thread', { threadTs }),
		getUser: (userId: string) => invoke<SlackUserSummary>('slack_get_user', { userId }),
		markRead: (channel: string, ts: string) => invoke<void>('slack_mark_read', { channel, ts }),
		postMessage: (channel: string, threadTs: string | null, text: string) =>
			invoke<SlackMessage>('slack_post_message', { channel, threadTs, text }),
	},
} as const;
