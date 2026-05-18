import { invoke } from '@tauri-apps/api/core';

import type {
	AppInfo,
	AppState,
	CoderModelSettings,
	CoderProviderConfig,
	CoderSessionSummary,
	CoderStatus,
	ProviderKind,
	ProviderModelSummary,
	ProviderProbeResult,
	RouterModel,
	ContainerStatus,
	ContentReplaceOptions,
	ContentReplaceResult,
	ContentSearchOptions,
	ContentSearchResult,
	DeviceCode,
	DirEntry,
	EditorConfig,
	FileSearchOptions,
	FileSearchResult,
	BranchList,
	BranchDiffStatus,
	CollectPathsResult,
	PrListScope,
	BranchSwitchTarget,
	GitBranchInfo,
	GitChangeSummary,
	GitCommitResult,
	GitFileBlame,
	GitStatusEntry,
	HfIdentity,
	ImageAttachmentPayload,
	LogEntry,
	LogLevel,
	LspCodeAction,
	LspCompletionItem,
	LspCompletionList,
	LspDiagnostic,
	LspHover,
	LspLocation,
	LspPosition,
	LspPrepareRename,
	LspRange,
	LspWorkspaceEdit,
	ProjectComposeStatus,
	ForwardedPort,
	ForwardedPortStatus,
	PortsApplyResult,
	ReadFileResult,
	RightPanelKind,
	SlackBotProfile,
	SlackIdentity,
	SlackMessage,
	SlackSession,
	SlackStatus,
	SlackUserSummary,
	StatResult,
	SystemTheme,
	TerminalOpenRequest,
	Workspace,
	WorkspaceMeta,
	WorkspaceSession,
	WriteFileResult,
	NextEditCompleteParams,
	NextEditCompleteResult,
	NextEditProbeResult,
	NextEditServerSnapshot,
	NextEditServerStartParams,
} from './protocol';

// Single auditable surface for all Tauri commands. Components MUST go
// through this. Process-per-workspace (Phase 7) made every workspace
// scoped command implicit on the process's own workspace, so no
// `workspaceId` is threaded through the wire — `invoke` everywhere.
export const ipc = {
	// `app_info` is the very first IPC the frontend issues on
	// hydrate to learn whether this process is in workspace mode
	// or in preboot (no `--workspace` arg, empty catalog).
	appInfo: () => invoke<AppInfo>('app_info'),
	workspace: {
		openLocal: (path: string) => invoke<Workspace>('workspace_open_local', { path }),
		removeFolder: (path: string) => invoke<Workspace>('workspace_remove_folder', { path }),
		setActiveFolder: (path: string) => invoke<Workspace>('workspace_set_active_folder', { path }),
		active: () => invoke<Workspace | null>('workspace_active'),
		list: () => invoke<Workspace[]>('workspace_list'),
	},
	// Catalog of every workspace on the machine + create / delete /
	// rename. Cross-process operations: writes to `state.json` are
	// visible to sibling processes the next time they re-read the
	// catalog (the picker re-reads on every open).
	workspaces: {
		catalog: () => invoke<WorkspaceMeta[]>('workspace_catalog'),
		// Idempotent create-or-switch: returns the existing
		// catalog entry if the slug is already taken, otherwise
		// appends a new one. Pass an empty `slug` to auto-derive
		// it from `name`. Pair with `window.open` for the
		// "Ctrl+Shift+N" flow — together they create a new
		// workspace or focus an existing one transparently.
		create: (slug: string, name: string) => invoke<WorkspaceMeta>('workspace_create', { slug, name }),
		delete: (slug: string) => invoke<void>('workspace_delete', { slug }),
		rename: (slug: string, name: string) => invoke<WorkspaceMeta>('workspace_rename', { slug, name }),
		// Update the badge colour for `slug`. Pass `''` (empty
		// string) to clear back to the deterministic hash-derived
		// colour. When `slug` matches the running process's own
		// workspace, the window icon repaints immediately —
		// sibling processes pick up the change on their next
		// launch.
		setColor: (slug: string, color: string) => invoke<WorkspaceMeta>('workspace_set_color', { slug, color }),
	},
	// Window management. Process-per-workspace: `open` either
	// focuses the sibling process that already owns `slug` or
	// spawns a fresh `moon-ide --workspace <slug>` child;
	// `close` exits this process; `setTitle` rewrites the
	// calling window's OS title.
	window: {
		open: (slug: string) => invoke<void>('window_open', { workspaceId: slug }),
		close: () => invoke<void>('window_close'),
		setTitle: (title: string) => invoke<void>('window_set_title', { title }),
	},
	fs: {
		readDir: (path: string) => invoke<DirEntry[]>('fs_read_dir', { path }),
		collectPaths: (maxDepth: number) => invoke<CollectPathsResult>('fs_collect_paths', { maxDepth }),
		collectPathsUnder: (rel: string, maxDepth: number) =>
			invoke<CollectPathsResult>('fs_collect_paths_under', { rel, maxDepth }),
		readFile: (path: string) => invoke<ReadFileResult>('fs_read_file', { path }),
		writeFile: (path: string, text: string) => invoke<WriteFileResult>('fs_write_file', { path, text }),
		// Host-direct read/write for files outside every bound folder. Bypasses
		// the active `WorkspaceHost` so an external path stays readable when the
		// active folder runs in a container (the in-container host can't see
		// paths outside the bind mount). See `Workspace::openHostFile`.
		readFileHost: (path: string) => invoke<ReadFileResult>('fs_read_file_host', { path }),
		writeFileHost: (path: string, text: string) => invoke<WriteFileResult>('fs_write_file_host', { path, text }),
		createFile: (path: string) => invoke<void>('fs_create_file', { path }),
		createDir: (path: string) => invoke<void>('fs_create_dir', { path }),
		rename: (from: string, to: string) => invoke<void>('fs_rename', { from, to }),
		stat: (path: string) => invoke<StatResult>('fs_stat', { path }),
		absolutePath: (path: string) => invoke<string>('fs_absolute_path', { path }),
		trash: (path: string) => invoke<void>('fs_trash', { path }),
		delete: (path: string) => invoke<void>('fs_delete', { path }),
		gitStatusEntries: (paths: string[]) => invoke<GitStatusEntry[]>('fs_git_status_entries', { paths }),
		gitChangeSummary: (folderPath: string) => invoke<GitChangeSummary>('fs_git_change_summary', { folderPath }),
		gitRestorePaths: (paths: string[]) => invoke<void>('fs_git_restore_paths', { paths }),
		gitBlame: (path: string) => invoke<GitFileBlame | null>('fs_git_blame', { path }),
		gitHeadContent: (path: string) => invoke<string | null>('fs_git_head_content', { path }),
		gitRefContent: (rev: string, path: string) => invoke<string | null>('fs_git_ref_content', { rev, path }),
		gitDefaultBranchDiff: () => invoke<BranchDiffStatus | null>('fs_git_default_branch_diff'),
		gitBranch: () => invoke<GitBranchInfo>('fs_git_branch'),
		gitCommit: (message: string, amend: boolean) => invoke<GitCommitResult>('fs_git_commit', { message, amend }),
		gitCommitOnNewBranch: (branch: string, message: string) =>
			invoke<GitCommitResult>('fs_git_commit_on_new_branch', { branch, message }),
		gitPush: () => invoke<void>('fs_git_push'),
		gitPublishBranch: () => invoke<void>('fs_git_publish_branch'),
		gitPull: () => invoke<void>('fs_git_pull'),
		gitMergeDefaultBranch: (remoteRef: string) => invoke<void>('fs_git_merge_default_branch', { remoteRef }),
		gitFetch: () => invoke<void>('fs_git_fetch'),
		gitHeadCommitMessage: () => invoke<string>('fs_git_head_commit_message'),
		branchList: (prScope: PrListScope) => invoke<BranchList>('fs_branch_list', { prScope }),
		branchSwitch: (target: BranchSwitchTarget) => invoke<void>('fs_branch_switch', { target }),
	},
	search: {
		files: (options: FileSearchOptions) => invoke<FileSearchResult[]>('search_files', { options }),
		content: (options: ContentSearchOptions) => invoke<ContentSearchResult>('search_content', { options }),
		replaceContent: (options: ContentReplaceOptions) =>
			invoke<ContentReplaceResult>('search_replace_content', { options }),
	},
	appState: {
		load: () => invoke<AppState>('app_state_load'),
		save: (appState: AppState) => invoke<void>('app_state_save', { appState }),
	},
	session: {
		// Per-workspace UI session blob (folders bound, tabs,
		// splits, focused folder, SCM filters). Lives at
		// `<workspaces_dir>/<id>/session.json`. Process-per-
		// workspace makes the workspace id implicit — the
		// backend reads it from `state.workspace_id()`.
		load: () => invoke<WorkspaceSession>('session_load'),
		save: (session: WorkspaceSession) => invoke<void>('session_save', { session }),
	},
	system: {
		theme: () => invoke<SystemTheme>('system_theme'),
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
		stop: () => invoke<ContainerStatus>('container_stop'),
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
	ports: {
		list: () => invoke<ForwardedPort[]>('ports_list'),
		set: (forwards: ForwardedPort[]) => invoke<PortsApplyResult>('ports_set', { forwards }),
		status: () => invoke<ForwardedPortStatus[]>('ports_status'),
		reapply: () => invoke<PortsApplyResult>('ports_reapply'),
	},
	composeLogs: {
		open: (folderPath: string, service: string) => invoke<string>('compose_logs_open', { folderPath, service }),
		close: (streamId: string) => invoke<void>('compose_logs_close', { streamId }),
	},
	terminal: {
		open: (request: TerminalOpenRequest) => invoke<string>('terminal_open', { request }),
		write: (streamId: string, data: string) => invoke<void>('terminal_write', { streamId, data }),
		resize: (streamId: string, cols: number, rows: number) => invoke<void>('terminal_resize', { streamId, cols, rows }),
		close: (streamId: string) => invoke<void>('terminal_close', { streamId }),
	},
	nextEdit: {
		probe: (baseUrl: string) => invoke<NextEditProbeResult>('next_edit_probe', { baseUrl: baseUrl.trim() }),
		complete: (params: NextEditCompleteParams) => invoke<NextEditCompleteResult>('next_edit_complete', { params }),
		serverStart: (params: NextEditServerStartParams) =>
			invoke<NextEditServerSnapshot>('next_edit_server_start', { params }),
		serverStop: () => invoke<NextEditServerSnapshot>('next_edit_server_stop'),
		serverStatus: () => invoke<NextEditServerSnapshot>('next_edit_server_status'),
	},
	logs: {
		snapshot: (source: string) => invoke<LogEntry[]>('logs_snapshot', { source }),
		sources: () => invoke<string[]>('logs_sources'),
		clear: (source: string) => invoke<void>('logs_clear', { source }),
		emit: (source: string, level: LogLevel, message: string) => invoke<void>('logs_emit', { source, level, message }),
	},
	lsp: {
		open: (path: string, languageId: string, text: string) => invoke<void>('lsp_open', { path, languageId, text }),
		update: (path: string, languageId: string, text: string) => invoke<void>('lsp_update', { path, languageId, text }),
		close: (path: string, languageId: string) => invoke<void>('lsp_close', { path, languageId }),
		hover: (path: string, languageId: string, position: LspPosition) =>
			invoke<LspHover | null>('lsp_hover', { path, languageId, position }),
		completion: (path: string, languageId: string, position: LspPosition) =>
			invoke<LspCompletionList>('lsp_completion', { path, languageId, position }),
		// Lazy-resolve a completion item (the auto-import block lives
		// here for `tsgo` / `rust-analyzer` / `pyright`). The
		// `resolveToken` is the opaque string the matching item
		// carried; we hand it back unchanged. Returns the resolved
		// item with `additionalTextEdits` filled in. When the
		// matching server didn't advertise resolveSupport, the
		// backend short-circuits to a no-op and gives us back the
		// item we'd already have — same shape, no IPC fan-out.
		completionResolve: (languageId: string, resolveToken: string) =>
			invoke<LspCompletionItem>('lsp_completion_resolve', { languageId, resolveToken }),
		definition: (path: string, languageId: string, position: LspPosition) =>
			invoke<LspLocation | null>('lsp_definition', { path, languageId, position }),
		prepareRename: (path: string, languageId: string, position: LspPosition, fallbackWord: string) =>
			invoke<LspPrepareRename | null>('lsp_prepare_rename', { path, languageId, position, fallbackWord }),
		rename: (path: string, languageId: string, position: LspPosition, newName: string) =>
			invoke<LspWorkspaceEdit>('lsp_rename', { path, languageId, position, newName }),
		// Quick-fix (`textDocument/codeAction`, `quickfix`-only) for
		// one diagnostic the cursor is parked on. `producer` is the
		// slot key the frontend received with the original
		// `lsp:diagnostics` event (`"typescript"` for tsgo,
		// `"oxlint"` for the linter co-tenant) — the broker uses it
		// to route the request to the server that warned about this
		// range, not the other JS/TS co-tenant. Returns an empty
		// list when nothing is wired or running; the lint tooltip
		// falls back to the always-on "Fix in coder" entry the
		// frontend layers on top of every diagnostic regardless of
		// what the server returned.
		codeAction: (path: string, producer: string, range: LspRange, diagnostic: LspDiagnostic) =>
			invoke<LspCodeAction[]>('lsp_code_action', { path, producer, range, diagnostic }),
		// Tear down the server slot for `languageId`. The next
		// `open` / `update` / `hover` / `completion` lazily
		// re-spawns it; the diag-logs panel exposes a "Restart"
		// button per `lsp.*` source. No-op when no broker exists
		// yet — calling restart before any LSP has spun up is
		// fine.
		restart: (languageId: string) => invoke<void>('lsp_restart', { languageId }),
		// Re-pull diagnostics for every document on every running
		// language server (optionally scoped to a subset of
		// languages). The frontend uses this on window-focus
		// events to cover the cold-start case the fs-watcher
		// structurally can't — a `git checkout` that happened
		// while the IDE was closed leaves no fs event to react
		// to. In-IDE off-disk changes flow through
		// `notifyFilesChanged` instead, so the server can
		// invalidate the right caches and trigger a refresh on
		// its own. Pass `[]` to refresh every running server.
		refreshOpenDiagnostics: (languageIds: readonly string[]) =>
			invoke<void>('lsp_refresh_open_diagnostics', { languageIds }),
		// Forward an fs-watcher batch to every running language
		// server as one `workspace/didChangeWatchedFiles`
		// notification per server, scoped through the globs that
		// server registered via `client/registerCapability`. The
		// canonical LSP plumbing for off-disk file changes —
		// well-behaved servers invalidate caches and fire a
		// `workspace/diagnostic/refresh` request back, which the
		// backend turns into a per-server diagnostic re-pull. No-op
		// when no broker has spun up yet, or when no server has
		// registered a watcher matching any of the paths.
		notifyFilesChanged: (paths: readonly string[]) => invoke<void>('lsp_notify_files_changed', { paths }),
	},
	slack: {
		setToken: (token: string) => invoke<SlackIdentity>('slack_set_token', { token }),
		status: () => invoke<SlackStatus>('slack_status'),
		clearToken: () => invoke<void>('slack_clear_token'),
		listDmBots: () => invoke<SlackBotProfile[]>('slack_list_dm_bots'),
		selectBot: (profile: SlackBotProfile) => invoke<void>('slack_select_bot', { profile }),
		clearBot: () => invoke<void>('slack_clear_bot'),
		getActiveBot: () => invoke<SlackBotProfile | null>('slack_get_active_bot'),
		setWindowFocused: (focused: boolean) => invoke<void>('slack_set_window_focused', { focused }),
		listSessions: (channel: string) => invoke<SlackSession[]>('slack_list_sessions', { channel }),
		getThread: (channel: string, threadTs: string) => invoke<SlackMessage[]>('slack_get_thread', { channel, threadTs }),
		setActiveThread: (threadTs: string | null) => invoke<void>('slack_set_active_thread', { threadTs }),
		getUser: (userId: string) => invoke<SlackUserSummary>('slack_get_user', { userId }),
		markRead: (channel: string, ts: string) => invoke<void>('slack_mark_read', { channel, ts }),
		postMessage: (channel: string, threadTs: string | null, text: string) =>
			invoke<SlackMessage>('slack_post_message', { channel, threadTs, text }),
	},
	coder: {
		status: () => invoke<CoderStatus>('coder_status'),
		folderSummary: (folder: string) => invoke<string | null>('coder_folder_summary', { folder }),
		startDeviceFlow: () => invoke<DeviceCode>('coder_start_device_flow'),
		pollDeviceCode: (code: DeviceCode) => invoke<HfIdentity>('coder_poll_device_code', { code }),
		signOut: () => invoke<void>('coder_sign_out'),
		send: (text: string, images: ImageAttachmentPayload[] = []) => invoke<void>('coder_send', { text, images }),
		suggestBranchName: (message: string) => invoke<string>('coder_suggest_branch_name', { message }),
		suggestCommitMessage: (message: string) => invoke<string>('coder_suggest_commit_message', { message }),
		abort: () => invoke<void>('coder_abort'),
		listSessions: () => invoke<CoderSessionSummary[]>('coder_list_sessions'),
		activeSession: () => invoke<CoderSessionSummary | null>('coder_active_session'),
		newSession: () => invoke<CoderSessionSummary>('coder_new_session'),
		openSession: (id: string) => invoke<CoderSessionSummary>('coder_open_session', { id }),
		deleteSession: (id: string) => invoke<void>('coder_delete_session', { id }),
		sessionJsonlPath: (id: string) => invoke<string>('coder_session_jsonl_path', { id }),
		getModelSettings: () => invoke<CoderModelSettings>('coder_get_model_settings'),
		setModelSettings: (settings: CoderModelSettings) => invoke<void>('coder_set_model_settings', { settings }),
		listModels: () => invoke<RouterModel[]>('coder_list_models'),
		listProviderModels: (id: string) => invoke<ProviderModelSummary[]>('coder_list_provider_models', { id }),
		newProviderId: () => invoke<string>('coder_new_provider_id'),
		probeProvider: (baseUrl: string, apiKey: string, kind: ProviderKind = 'custom') =>
			invoke<ProviderProbeResult>('coder_probe_provider', {
				args: { base_url: baseUrl, api_key: apiKey, kind },
			}),
		saveProvider: (config: CoderProviderConfig) => invoke<void>('coder_save_provider', { config }),
		deleteProvider: (id: string) => invoke<void>('coder_delete_provider', { id }),
		setProviderApiKey: (id: string, key: string) => invoke<void>('coder_set_provider_api_key', { args: { id, key } }),
		clearProviderApiKey: (id: string) => invoke<void>('coder_clear_provider_api_key', { id }),
		webSearchConfigured: () => invoke<boolean>('coder_web_search_configured'),
		setWebSearchKey: (key: string) => invoke<void>('coder_set_web_search_key', { key }),
		clearWebSearchKey: () => invoke<void>('coder_clear_web_search_key'),
	},
	ui: {
		setRightPanel: (kind: RightPanelKind | null) => invoke<void>('ui_set_right_panel', { kind }),
	},
} as const;
