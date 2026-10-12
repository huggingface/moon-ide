import { call, local } from './transport';

import type {
	AppInfo,
	AppState,
	BrowserTab,
	CoderHubBucket,
	CoderModelSettings,
	CoderProviderConfig,
	CoderSessionSummary,
	CoderStatus,
	HubNamespace,
	HubUploadAllSummary,
	McpServerStatus,
	OpenRouterCredits,
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
	CommitEntry,
	CommitDiff,
	GitBranchInfo,
	GitChangeSummary,
	GitCommitResult,
	GitFileBlame,
	GitMergeState,
	GitPermalink,
	GitStatusEntry,
	GitWorktree,
	HfIdentity,
	ImageAttachmentPayload,
	UnqueuedSteer,
	RevertedMessage,
	RerunToolOutcome,
	AttachWorkerOutcome,
	BackgroundProcessSnapshot,
	DisconnectWorkerOutcome,
	FleetMember,
	WorkerLinkState,
	PromptResponse,
	LogEntry,
	LogLevel,
	LspCodeAction,
	LspCompletionItem,
	LspCompletionList,
	LspDiagnostic,
	LspFileChange,
	LspHover,
	LspLocation,
	LspPosition,
	LspPrepareRename,
	LspRange,
	LspWorkspaceEdit,
	NewWorktreeSession,
	ProjectComposeStatus,
	ForwardedPort,
	ForwardedPortStatus,
	PortsApplyResult,
	PublishReviewRequest,
	PublishReviewResult,
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

// Single auditable surface for all workspace-process commands. Components
// MUST go through this. Process-per-workspace (Phase 7) made every
// workspace scoped command implicit on the process's own workspace, so no
// `workspaceId` is threaded through the wire.
export const ipc = {
	// `app_info` is the very first IPC the frontend issues on
	// hydrate to learn whether this process is in workspace mode
	// or in preboot (no `--workspace` arg, empty catalog).
	appInfo: () => local<AppInfo>('app_info'),
	workspace: {
		openLocal: (path: string) => local<Workspace>('workspace_open_local', { path }),
		removeFolder: (path: string) => local<Workspace>('workspace_remove_folder', { path }),
		setActiveFolder: (path: string) => local<Workspace>('workspace_set_active_folder', { path }),
		syncActiveWorktreeBranch: () => local<Workspace>('workspace_sync_active_worktree_branch'),
		active: () => local<Workspace | null>('workspace_active'),
		list: () => local<Workspace[]>('workspace_list'),
	},
	// Catalog of every workspace on the machine + create / delete /
	// rename. Cross-process operations: writes to `state.json` are
	// visible to sibling processes the next time they re-read the
	// catalog (the picker re-reads on every open).
	workspaces: {
		catalog: () => local<WorkspaceMeta[]>('workspace_catalog'),
		// Idempotent create-or-switch: returns the existing
		// catalog entry if the slug is already taken, otherwise
		// appends a new one. Pass an empty `slug` to auto-derive
		// it from `name`. Pair with `window.open` for the
		// "Ctrl+Shift+N" flow — together they create a new
		// workspace or focus an existing one transparently.
		create: (slug: string, name: string) => local<WorkspaceMeta>('workspace_create', { slug, name }),
		delete: (slug: string) => local<void>('workspace_delete', { slug }),
		rename: (slug: string, name: string) => local<WorkspaceMeta>('workspace_rename', { slug, name }),
		// Update the badge colour for `slug`. Pass `''` (empty
		// string) to clear back to the deterministic hash-derived
		// colour. When `slug` matches the running process's own
		// workspace, the window icon repaints immediately —
		// sibling processes pick up the change on their next
		// launch.
		setColor: (slug: string, color: string) => local<WorkspaceMeta>('workspace_set_color', { slug, color }),
	},
	// Window management. Process-per-workspace: `open` either
	// focuses the sibling process that already owns `slug` or
	// spawns a fresh `moon-ide --workspace <slug>` child;
	// `close` exits this process; `setTitle` rewrites the
	// calling window's OS title.
	window: {
		open: (slug: string) => local<void>('window_open', { workspaceId: slug }),
		close: () => local<void>('window_close'),
		setTitle: (title: string) => local<void>('window_set_title', { title }),
	},
	fs: {
		readDir: (path: string) => call<DirEntry[]>('fs_read_dir', { path }),
		collectPaths: (maxDepth: number) => call<CollectPathsResult>('fs_collect_paths', { maxDepth }),
		collectPathsUnder: (rel: string, maxDepth: number) =>
			call<CollectPathsResult>('fs_collect_paths_under', { rel, maxDepth }),
		readFile: (path: string) => call<ReadFileResult>('fs_read_file', { path }),
		writeFile: (path: string, text: string) => call<WriteFileResult>('fs_write_file', { path, text }),
		// Host-direct read/write for files outside every bound folder. Bypasses
		// the active `WorkspaceHost` so an external path stays readable when the
		// active folder runs in a container (the in-container host can't see
		// paths outside the bind mount). See `Workspace::openHostFile`.
		readFileHost: (path: string) => call<ReadFileResult>('fs_read_file_host', { path }),
		writeFileHost: (path: string, text: string) => call<WriteFileResult>('fs_write_file_host', { path, text }),
		createFile: (path: string) => call<void>('fs_create_file', { path }),
		createDir: (path: string) => call<void>('fs_create_dir', { path }),
		rename: (from: string, to: string) => call<void>('fs_rename', { from, to }),
		stat: (path: string) => call<StatResult>('fs_stat', { path }),
		absolutePath: (path: string) => call<string>('fs_absolute_path', { path }),
		// Returns a `data:` URL. Only for previews outside every bound
		// folder — in-workspace files stream via `fileUrl`.
		readPreviewHost: (path: string) => call<string>('fs_read_preview_host', { path }),
		trash: (path: string) => call<void>('fs_trash', { path }),
		delete: (path: string) => call<void>('fs_delete', { path }),
		// Reveal an absolute host path in the host's file manager
		// (Explorer / Finder / FileManager1 dbus). Callers resolve the
		// workspace-relative path via `fs_absolute_path` first.
		revealInFolder: (path: string) => local<void>('fs_reveal_in_folder', { path }),
		gitStatusEntries: (paths: string[]) => call<GitStatusEntry[]>('fs_git_status_entries', { paths }),
		gitExcludePath: () => call<string | null>('fs_git_exclude_path'),
		gitChangeSummary: (folderPath: string) => call<GitChangeSummary>('fs_git_change_summary', { folderPath }),
		gitRestorePaths: (paths: string[]) => call<void>('fs_git_restore_paths', { paths }),
		gitAddPaths: (paths: string[]) => call<void>('fs_git_add_paths', { paths }),
		gitBlame: (path: string) => call<GitFileBlame | null>('fs_git_blame', { path }),
		gitPermalink: (path: string, startLine: number, endLine: number) =>
			call<GitPermalink | null>('fs_git_permalink', { path, startLine, endLine }),
		gitBlobSha: (path: string) => call<string | null>('fs_git_blob_sha', { path }),
		publishPrReview: (request: PublishReviewRequest) => call<PublishReviewResult>('fs_publish_pr_review', { request }),
		gitHeadContent: (path: string) => call<string | null>('fs_git_head_content', { path }),
		gitRefContent: (rev: string, path: string) => call<string | null>('fs_git_ref_content', { rev, path }),
		gitDefaultBranchDiff: () => call<BranchDiffStatus | null>('fs_git_default_branch_diff'),
		gitRemoteWebUrl: () => call<string | null>('fs_git_remote_web_url'),
		gitBranch: () => call<GitBranchInfo>('fs_git_branch'),
		gitCommit: (message: string, amend: boolean) => call<GitCommitResult>('fs_git_commit', { message, amend }),
		gitCommitOnNewBranch: (branch: string, message: string) =>
			call<GitCommitResult>('fs_git_commit_on_new_branch', { branch, message }),
		gitWorktreeAdd: (path: string, branch: string) => call<GitWorktree>('fs_git_worktree_add', { path, branch }),
		gitWorktreeList: () => call<GitWorktree[]>('fs_git_worktree_list'),
		gitWorktreeRemove: (path: string, force: boolean) => call<void>('fs_git_worktree_remove', { path, force }),
		gitPush: () => call<void>('fs_git_push'),
		gitPublishBranch: () => call<void>('fs_git_publish_branch'),
		gitPull: () => call<void>('fs_git_pull'),
		gitMergeDefaultBranch: (remoteRef: string) => call<void>('fs_git_merge_default_branch', { remoteRef }),
		gitMergeState: () => call<GitMergeState>('fs_git_merge_state'),
		gitMergeAbort: () => call<void>('fs_git_merge_abort'),
		gitFetch: () => call<void>('fs_git_fetch'),
		gitHeadCommitMessage: () => call<string>('fs_git_head_commit_message'),
		gitLog: (limit?: number) => call<CommitEntry[]>('fs_git_log', { limit: limit ?? null }),
		gitCommitDiff: (sha: string) => call<CommitDiff | null>('fs_git_commit_diff', { sha }),
		branchList: (prScope: PrListScope) => call<BranchList>('fs_branch_list', { prScope }),
		gitExistingPrUrl: () => call<string | null>('fs_git_existing_pr_url'),
		branchSwitch: (target: BranchSwitchTarget) => call<void>('fs_branch_switch', { target }),
	},
	search: {
		files: (options: FileSearchOptions) => call<FileSearchResult[]>('search_files', { options }),
		content: (options: ContentSearchOptions) => call<ContentSearchResult>('search_content', { options }),
		replaceContent: (options: ContentReplaceOptions) =>
			call<ContentReplaceResult>('search_replace_content', { options }),
	},
	appState: {
		load: () => local<AppState>('app_state_load'),
		save: (appState: AppState) => local<void>('app_state_save', { appState }),
	},
	session: {
		// Per-workspace UI session blob (folders bound, tabs,
		// splits, focused folder, SCM filters). Lives at
		// `<workspaces_dir>/<id>/session.json`. Process-per-
		// workspace makes the workspace id implicit — the
		// backend reads it from `state.workspace_id()`.
		load: () => call<WorkspaceSession>('session_load'),
		save: (session: WorkspaceSession) => call<void>('session_save', { session }),
	},
	system: {
		theme: () => local<SystemTheme>('system_theme'),
	},
	editorconfig: {
		forPath: (path: string) => call<EditorConfig>('editorconfig_for_path', { path }),
	},
	editorForward: {
		// Resolve a pending `editor:request` event from the
		// in-container `moon-edit` shim. `finish` writes the
		// buffer's bytes first (caller's responsibility) and
		// then drops this so the shim sees `OK\n` and `git`
		// proceeds. `cancel` sends `CANCEL\n` so `git` aborts.
		// Returns `false` if the id was unknown (already
		// resolved, listener gone) — both cases are benign,
		// the frontend treats them as success. See ADR 0021.
		finish: (id: string) => local<boolean>('editor_forward_finish', { id }),
		cancel: (id: string) => local<boolean>('editor_forward_cancel', { id }),
	},
	container: {
		status: () => local<ContainerStatus>('container_status'),
		setup: () => local<ContainerStatus>('container_setup'),
		pause: () => local<ContainerStatus>('container_pause'),
		resume: () => local<ContainerStatus>('container_resume'),
		rebuild: () => local<ContainerStatus>('container_rebuild'),
		stop: () => local<ContainerStatus>('container_stop'),
		teardown: () => local<ContainerStatus>('container_teardown'),
		applyBoundFolders: () => local<ContainerStatus>('container_apply_bound_folders'),
		renderCompose: () => local<string>('container_render_compose'),
	},
	projectCompose: {
		status: (folderPath: string) => local<ProjectComposeStatus>('project_compose_status', { folderPath }),
		up: (folderPath: string) => local<ProjectComposeStatus>('project_compose_up', { folderPath }),
		pause: (folderPath: string) => local<ProjectComposeStatus>('project_compose_pause', { folderPath }),
		resume: (folderPath: string) => local<ProjectComposeStatus>('project_compose_resume', { folderPath }),
		rebuild: (folderPath: string) => local<ProjectComposeStatus>('project_compose_rebuild', { folderPath }),
		stop: (folderPath: string) => local<ProjectComposeStatus>('project_compose_stop', { folderPath }),
		down: (folderPath: string) => local<ProjectComposeStatus>('project_compose_down', { folderPath }),
		serviceStart: (folderPath: string, service: string) =>
			local<ProjectComposeStatus>('project_compose_service_start', { folderPath, service }),
		serviceStop: (folderPath: string, service: string) =>
			local<ProjectComposeStatus>('project_compose_service_stop', { folderPath, service }),
		serviceRestart: (folderPath: string, service: string) =>
			local<ProjectComposeStatus>('project_compose_service_restart', { folderPath, service }),
		serviceRecreate: (folderPath: string, service: string) =>
			local<ProjectComposeStatus>('project_compose_service_recreate', { folderPath, service }),
	},
	browser: {
		/** Turn a URL into one the webview can load: unchanged for
		 *  `inContainer = false`, otherwise rewritten onto a host
		 *  loopback tunnel that dials from inside the workspace
		 *  shell (ADR 0088). */
		resolveUrl: (url: string, inContainer: boolean) => local<string>('browser_resolve_url', { url, inContainer }),
		// Tab registry (ADR 0088). Mutations answer via `browser:tabs`.
		list: () => local<BrowserTab[]>('browser_tabs_list'),
		open: (url: string, inContainer: boolean, reuse: boolean) =>
			local<BrowserTab>('browser_tab_open', { url, inContainer, reuse }),
		navigate: (id: number, url: string) => local<BrowserTab>('browser_tab_navigate', { id, url }),
		reload: (id: number) => local<void>('browser_tab_reload', { id }),
		focus: (id: number) => local<void>('browser_tab_focus', { id }),
		close: (id: number) => local<void>('browser_tab_close', { id }),
		location: (id: number, url: string) => local<void>('browser_tab_location', { id, url }),
		/** Answer a `browser:page_request` (`error` non-null = failed). */
		respond: (requestId: number, value: unknown, error: string | null) =>
			local<void>('browser_page_respond', { requestId, value, error }),
	},
	ports: {
		list: () => local<ForwardedPort[]>('ports_list'),
		set: (forwards: ForwardedPort[]) => local<PortsApplyResult>('ports_set', { forwards }),
		status: () => local<ForwardedPortStatus[]>('ports_status'),
		reapply: () => local<PortsApplyResult>('ports_reapply'),
	},
	composeLogs: {
		open: (folderPath: string, service: string) => local<string>('compose_logs_open', { folderPath, service }),
		close: (streamId: string) => local<void>('compose_logs_close', { streamId }),
	},
	terminal: {
		open: (request: TerminalOpenRequest) => local<string>('terminal_open', { request }),
		write: (streamId: string, data: string) => local<void>('terminal_write', { streamId, data }),
		resize: (streamId: string, cols: number, rows: number) => local<void>('terminal_resize', { streamId, cols, rows }),
		close: (streamId: string) => local<void>('terminal_close', { streamId }),
	},
	nextEdit: {
		probe: (baseUrl: string) => local<NextEditProbeResult>('next_edit_probe', { baseUrl: baseUrl.trim() }),
		complete: (params: NextEditCompleteParams) => local<NextEditCompleteResult>('next_edit_complete', { params }),
		serverStart: (params: NextEditServerStartParams) =>
			local<NextEditServerSnapshot>('next_edit_server_start', { params }),
		serverStop: () => local<NextEditServerSnapshot>('next_edit_server_stop'),
		serverStatus: () => local<NextEditServerSnapshot>('next_edit_server_status'),
	},
	logs: {
		snapshot: (source: string) => local<LogEntry[]>('logs_snapshot', { source }),
		sources: () => local<string[]>('logs_sources'),
		clear: (source: string) => local<void>('logs_clear', { source }),
		emit: (source: string, level: LogLevel, message: string) => local<void>('logs_emit', { source, level, message }),
	},
	lsp: {
		open: (path: string, languageId: string, text: string) => local<void>('lsp_open', { path, languageId, text }),
		update: (path: string, languageId: string, text: string) => local<void>('lsp_update', { path, languageId, text }),
		close: (path: string, languageId: string) => local<void>('lsp_close', { path, languageId }),
		hover: (path: string, languageId: string, position: LspPosition) =>
			local<LspHover | null>('lsp_hover', { path, languageId, position }),
		completion: (path: string, languageId: string, position: LspPosition) =>
			local<LspCompletionList>('lsp_completion', { path, languageId, position }),
		// Lazy-resolve a completion item (the auto-import block lives
		// here for `tsgo` / `rust-analyzer` / `pyright`). The
		// `resolveToken` is the opaque string the matching item
		// carried; we hand it back unchanged. Returns the resolved
		// item with `additionalTextEdits` filled in. When the
		// matching server didn't advertise resolveSupport, the
		// backend short-circuits to a no-op and gives us back the
		// item we'd already have — same shape, no IPC fan-out.
		completionResolve: (languageId: string, resolveToken: string) =>
			local<LspCompletionItem>('lsp_completion_resolve', { languageId, resolveToken }),
		definition: (path: string, languageId: string, position: LspPosition) =>
			local<LspLocation | null>('lsp_definition', { path, languageId, position }),
		prepareRename: (path: string, languageId: string, position: LspPosition, fallbackWord: string) =>
			local<LspPrepareRename | null>('lsp_prepare_rename', { path, languageId, position, fallbackWord }),
		rename: (path: string, languageId: string, position: LspPosition, newName: string) =>
			local<LspWorkspaceEdit>('lsp_rename', { path, languageId, position, newName }),
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
			local<LspCodeAction[]>('lsp_code_action', { path, producer, range, diagnostic }),
		// Tear down the server slot for `languageId`. The next
		// `open` / `update` / `hover` / `completion` lazily
		// re-spawns it; the diag-logs panel exposes a "Restart"
		// button per `lsp.*` source. No-op when no broker exists
		// yet — calling restart before any LSP has spun up is
		// fine.
		restart: (languageId: string) => local<void>('lsp_restart', { languageId }),
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
			local<void>('lsp_refresh_open_diagnostics', { languageIds }),
		// Forward an fs-watcher batch to every running language
		// server as one `workspace/didChangeWatchedFiles`
		// notification per server, scoped through the globs that
		// server registered via `client/registerCapability`. The
		// canonical LSP plumbing for off-disk file changes —
		// well-behaved servers invalidate caches and fire a
		// `workspace/diagnostic/refresh` request back, which the
		// backend turns into a per-server diagnostic re-pull. Each
		// entry carries its change kind so a branch-switch delete
		// is reported as `Deleted` (servers drop the snapshot
		// instead of re-reading a vanished file and keeping stale
		// bytes). No-op when no broker has spun up yet, or when no
		// server has registered a watcher matching any of the
		// paths.
		notifyFilesChanged: (changes: readonly LspFileChange[]) => local<void>('lsp_notify_files_changed', { changes }),
	},
	slack: {
		setToken: (token: string) => local<SlackIdentity>('slack_set_token', { token }),
		status: () => local<SlackStatus>('slack_status'),
		clearToken: () => local<void>('slack_clear_token'),
		listDmBots: () => local<SlackBotProfile[]>('slack_list_dm_bots'),
		selectBot: (profile: SlackBotProfile) => local<void>('slack_select_bot', { profile }),
		clearBot: () => local<void>('slack_clear_bot'),
		getActiveBot: () => local<SlackBotProfile | null>('slack_get_active_bot'),
		setWindowFocused: (focused: boolean) => local<void>('slack_set_window_focused', { focused }),
		listSessions: (channel: string) => local<SlackSession[]>('slack_list_sessions', { channel }),
		getThread: (channel: string, threadTs: string) => local<SlackMessage[]>('slack_get_thread', { channel, threadTs }),
		setActiveThread: (threadTs: string | null) => local<void>('slack_set_active_thread', { threadTs }),
		getUser: (userId: string) => local<SlackUserSummary>('slack_get_user', { userId }),
		markRead: (channel: string, ts: string) => local<void>('slack_mark_read', { channel, ts }),
		postMessage: (channel: string, threadTs: string | null, text: string) =>
			local<SlackMessage>('slack_post_message', { channel, threadTs, text }),
	},
	coder: {
		status: () => local<CoderStatus>('coder_status'),
		folderSummary: (folder: string) => local<string | null>('coder_folder_summary', { folder }),
		startDeviceFlow: () => local<DeviceCode>('coder_start_device_flow'),
		pollDeviceCode: (code: DeviceCode) => local<HfIdentity>('coder_poll_device_code', { code }),
		signOut: () => local<void>('coder_sign_out'),
		send: (text: string, images: ImageAttachmentPayload[] = [], sessionId: string | null = null) =>
			local<void>('coder_send', { text, images, sessionId }),
		suggestBranchName: (message: string) => local<string>('coder_suggest_branch_name', { message }),
		suggestCommitMessage: (message: string) => local<string>('coder_suggest_commit_message', { message }),
		suggestTerminalCommand: (request: string, targetKind: string, cwd: string) =>
			local<string>('coder_suggest_terminal_command', { request, targetKind, cwd }),
		abort: () => local<void>('coder_abort'),
		drainSteerNow: (id: string) => local<boolean>('coder_drain_steer_now', { id }),
		unqueueSteer: (id: string) => local<UnqueuedSteer | null>('coder_unqueue_steer', { id }),
		continueSubagent: (subagentId: string, text: string) =>
			local<boolean>('coder_continue_subagent', { subagentId, text }),
		abortSubagent: (subagentId: string) => local<boolean>('coder_abort_subagent', { subagentId }),
		respondToPrompt: (callId: string, response: PromptResponse) =>
			local<boolean>('coder_respond_to_prompt', { callId, response }),
		revertToMessage: (userOrdinal: number) => local<RevertedMessage>('coder_revert_to_message', { userOrdinal }),
		replayFromMessage: (userOrdinal: number) => local<void>('coder_replay_from_message', { userOrdinal }),
		resumeFromToolCall: (toolCallId: string) => local<void>('coder_resume_from_tool_call', { toolCallId }),
		retryLastTurn: () => local<void>('coder_retry_last_turn'),
		rerunToolCall: (toolCallId: string) => local<RerunToolOutcome>('coder_rerun_tool_call', { toolCallId }),
		listSessions: () => local<CoderSessionSummary[]>('coder_list_sessions'),
		runningSessions: () => local<string[]>('coder_running_sessions'),
		searchSessions: (query: string) => local<string[]>('coder_search_sessions', { query }),
		activeSession: () => local<CoderSessionSummary | null>('coder_active_session'),
		lastOpenedSession: () => local<string | null>('coder_last_opened_session'),
		newSession: () => local<CoderSessionSummary>('coder_new_session'),
		newCoordinatorSession: () => local<CoderSessionSummary>('coder_new_coordinator_session'),
		workerLinkState: (sessionId: string) => local<WorkerLinkState>('coder_worker_link_state', { sessionId }),
		disconnectWorker: (sessionId: string) => local<DisconnectWorkerOutcome>('coder_disconnect_worker', { sessionId }),
		attachWorker: (coordinatorId: string, sessionId: string) =>
			local<AttachWorkerOutcome>('coder_attach_worker', { coordinatorId, sessionId }),
		attachedWorkers: (coordinatorId: string) => local<FleetMember[]>('coder_attached_workers', { coordinatorId }),
		readBackgroundProcess: (sessionId: string, id: string) =>
			local<BackgroundProcessSnapshot>('coder_read_background_process', { sessionId, id }),
		stopBackgroundProcess: (sessionId: string, id: string) =>
			local<unknown>('coder_stop_background_process', { sessionId, id }),
		newWorktreeSession: (baseBranch?: string) =>
			local<NewWorktreeSession>('coder_new_worktree_session', { baseBranch: baseBranch ?? null }),
		discardWorktree: (path: string, force: boolean) => local<Workspace>('coder_discard_worktree', { path, force }),
		mergeAndRemoveWorktree: (path: string, baseBranch: string) =>
			local<Workspace>('coder_merge_and_remove_worktree', { path, baseBranch }),
		associateBranch: () => local<CoderSessionSummary | null>('coder_associate_branch'),
		moveSessionToWorktree: () => local<NewWorktreeSession>('coder_move_session_to_worktree'),
		setBashTargetOverride: (forceHost: boolean) => local<boolean>('coder_set_bash_target_override', { forceHost }),
		openSession: (id: string) => local<CoderSessionSummary>('coder_open_session', { id }),
		deleteSession: (id: string) => local<void>('coder_delete_session', { id }),
		renameSession: (id: string, title: string) => local<CoderSessionSummary>('coder_rename_session', { id, title }),
		sessionJsonlPath: (id: string) => local<string>('coder_session_jsonl_path', { id }),
		getModelSettings: () => local<CoderModelSettings>('coder_get_model_settings'),
		setModelSettings: (settings: CoderModelSettings) => local<void>('coder_set_model_settings', { settings }),
		listModels: () => local<RouterModel[]>('coder_list_models'),
		listProviderModels: (id: string) => local<ProviderModelSummary[]>('coder_list_provider_models', { id }),
		openrouterCredits: (id: string) => local<OpenRouterCredits>('coder_openrouter_credits', { id }),
		newProviderId: () => local<string>('coder_new_provider_id'),
		probeProvider: (baseUrl: string, apiKey: string, kind: ProviderKind = 'custom') =>
			local<ProviderProbeResult>('coder_probe_provider', {
				args: { base_url: baseUrl, api_key: apiKey, kind },
			}),
		saveProvider: (config: CoderProviderConfig) => local<void>('coder_save_provider', { config }),
		deleteProvider: (id: string) => local<void>('coder_delete_provider', { id }),
		setProviderApiKey: (id: string, key: string) => local<void>('coder_set_provider_api_key', { args: { id, key } }),
		clearProviderApiKey: (id: string) => local<void>('coder_clear_provider_api_key', { id }),
		webSearchConfigured: () => local<boolean>('coder_web_search_configured'),
		setWebSearchKey: (key: string) => local<void>('coder_set_web_search_key', { key }),
		clearWebSearchKey: () => local<void>('coder_clear_web_search_key'),
		hubListNamespaces: () => local<HubNamespace[]>('coder_hub_list_namespaces'),
		hubGetBinding: () => local<CoderHubBucket | null>('coder_hub_get_binding'),
		hubCreateBucket: (namespace: string, name: string, isPrivate: boolean) =>
			local<CoderHubBucket>('coder_hub_create_bucket', { namespace, name, private: isPrivate }),
		hubSetAutosync: (enabled: boolean) => local<void>('coder_hub_set_autosync', { enabled }),
		hubDisconnect: () => local<void>('coder_hub_disconnect'),
		hubUploadSession: (sessionId: string) => local<void>('coder_hub_upload_session', { sessionId }),
		hubUploadAllSessions: () => local<HubUploadAllSummary>('coder_hub_upload_all_sessions'),
		hubSessionUrl: (sessionId: string) => local<string>('coder_hub_session_url', { sessionId }),
		mcpServers: () => local<McpServerStatus[]>('coder_mcp_servers'),
		mcpSetEnabled: (id: string, enabled: boolean) => local<void>('coder_mcp_set_enabled', { id, enabled }),
		mcpAddCustom: (server: {
			label: string;
			command: string;
			args: string[];
			env: Record<string, string>;
			description: string;
		}) => local<McpServerStatus[]>('coder_mcp_add_custom', { server }),
		mcpRemoveCustom: (id: string) => local<McpServerStatus[]>('coder_mcp_remove_custom', { id }),
	},
	ui: {
		setRightPanel: (kind: RightPanelKind | null) => local<void>('ui_set_right_panel', { kind }),
	},
	companion: {
		status: () => local<CompanionStatus>('companion_status'),
		revokeDevice: (deviceId: string) => local<void>('companion_revoke_device', { deviceId }),
		revokeIde: (ideId: string) => local<void>('companion_revoke_ide', { ideId }),
		pairCode: () => local<PairingQr>('companion_pair_code'),
		enroll: (bridgeUrl: string, code: string, label: string) =>
			local<void>('companion_enroll', { bridgeUrl, code, label }),
		remoteStatus: () => local<RemoteBridgeStatus>('companion_remote_status'),
		remoteDisconnect: () => local<void>('companion_remote_disconnect'),
		remotePairCode: () => local<PairingQr>('companion_remote_pair_code'),
		e2eDevices: () => local<AuthorizedPhone[]>('companion_e2e_devices'),
		e2eRevoke: (deviceId: string) => local<void>('companion_e2e_revoke', { deviceId }),
	},
} as const;

/** A phone this host authorized end-to-end (ADR 0087). This list, not
 * the relay's routing tokens, decides who can drive the IDE. */
export type AuthorizedPhone = {
	id: string;
	label: string;
	public_key: string;
	paired_at_ms: number;
};

/** A phone-pairing link minted on this IDE's request (ADR 0087).
 * `payload` is the end-to-end pairing link the QR encodes and the
 * panel offers for copy-paste; `code` is the relay routing code it
 * embeds. */
export type PairingQr = {
	payload: string;
	url: string;
	code: string;
	fingerprint: string;
};

/** Mobile-companion bridge status, read from the bridge's published
 * status file (Phase 13.4b). `running: false` when the bridge isn't up.
 * The `ides` field is the enrolled-IDE list (Phase 14, ADR 0031). */
export type CompanionStatus = {
	running: boolean;
	/** The wss:// URL phones connect to. Pairing codes are minted on
	 * demand via `pairCode`, not at bridge startup. */
	url: string;
	mdns_url: string | null;
	fingerprint: string;
	devices: { id: string; label: string; paired_at_ms: number }[];
	ides?: { id: string; label: string; enrolled_at_ms: number }[];
	connected_phones?: number;
};

/** Remote / relay bridge connection status (Phase 14.3, ADR 0031).
 * `connected: false` when the IDE isn't connected to a remote bridge
 * (local mode). */
export type RemoteBridgeStatus = {
	connected: boolean;
	bridge_url: string;
	ide_id: string;
	error: string | null;
	connected_phones: number;
};
