// Mirrors `crates/moon-protocol`. Keep in sync until ts-rs codegen is wired up.
//
// See specs/protocol.md.

export type EntryKind = 'file' | 'dir' | 'symlink' | 'other';

export type DirEntry = {
	name: string;
	path: string;
	kind: EntryKind;
	size: number | null;
	mtime_ms: number | null;
	is_hidden: boolean;
};

export type ReadFileResult = {
	text: string;
	mtime_ms: number | null;
	is_binary: boolean;
};

export type WriteFileResult = {
	mtime_ms: number | null;
	bytes_written: number;
};

export type StatResult = {
	kind: EntryKind;
	size: number;
	mtime_ms: number | null;
};

export type HostKind = 'local' | 'devcontainer';

/**
 * One folder bound into a workspace. Mirrors
 * `moon_protocol::workspace::WorkspaceFolder`.
 */
export type WorkspaceFolder = {
	path: string;
	name: string;
	host: HostKind;
};

/**
 * The full workspace shape — a singleton `"default"` workspace
 * holding zero or more folders, with at most one currently active.
 * Mirrors `moon_protocol::workspace::Workspace`.
 */
export type Workspace = {
	id: string;
	folders: WorkspaceFolder[];
	active_folder: string | null;
};

export type FileSearchOptions = {
	query: string;
	limit?: number;
};

export type FileSearchResult = {
	path: string;
	score: number;
};

export type ContentSearchOptions = {
	query: string;
	case_sensitive?: boolean;
	regex?: boolean;
	max_matches?: number;
	max_files?: number;
};

export type ContentSearchHit = {
	path: string;
	line: number;
	column: number;
	line_text: string;
	match_start: number;
	match_end: number;
};

export type ContentSearchResult = {
	hits: ContentSearchHit[];
	truncated: boolean;
};

export type ThemeMode = 'dark' | 'light';

export type SplitSide = 'left' | 'right';

export type IndentStyle = 'tab' | 'space';

export type EndOfLine = 'lf' | 'crlf' | 'cr';

/**
 * Fully resolved editorconfig for one file. Mirrors `moon_protocol::editorconfig::EditorConfig`.
 * The host walks `.editorconfig` from the file up to the workspace root and
 * returns this struct — callers don't traverse the cascade themselves.
 */
export type EditorConfig = {
	indent_style: IndentStyle;
	indent_size: number;
	tab_width: number;
	end_of_line: EndOfLine | null;
	insert_final_newline: boolean;
	trim_trailing_whitespace: boolean;
	charset: string;
	max_line_length: number | null;
};

/**
 * Same defaults as `EditorConfig::default()` in moon-protocol. Surfaced
 * to the editor when the host hasn't answered yet (first paint of a
 * fresh tab) so we don't flicker between two indentation regimes.
 */
export const defaultEditorConfig: EditorConfig = {
	indent_style: 'tab',
	indent_size: 2,
	tab_width: 2,
	end_of_line: 'lf',
	insert_final_newline: true,
	trim_trailing_whitespace: true,
	charset: 'utf-8',
	max_line_length: null,
};

/**
 * One folder's slice of UI state. Mirrors
 * `moon_protocol::session::FolderSession`. Tab paths are
 * folder-relative (relative to `folder_path`); the two
 * `open_files_*` lists are independent — a path can live in one
 * pane, both, or neither (VSCode/Zed convention).
 */
export type FolderSession = {
	folder_path: string;
	open_files_left: string[];
	open_files_right: string[];
	active_left: string | null;
	active_right: string | null;
	has_split: boolean;
	focused_side: SplitSide;
};

/**
 * Persisted UI session for the singleton workspace. Frontend-owned
 * shape; the backend is pure storage. Mirrors
 * `moon_protocol::session::WorkspaceSession`. Holds one
 * [`FolderSession`] per bound folder, plus a pointer to which folder
 * was active at last save.
 */
export type WorkspaceSession = {
	folders: FolderSession[];
	active_folder_path: string | null;
};

/**
 * Slack-specific slice of [`AppState`]. Only stores derived,
 * non-secret pointers — the `xoxp-` token itself stays in the OS
 * keyring. Mirrors `moon_protocol::app_state::SlackAppState`.
 */
export type SlackAppState = {
	active_bot: SlackBotProfile | null;
	panel_visible: boolean;
	active_thread_ts: string | null;
};

/**
 * Per-machine, per-user app state. There is intentionally no `Settings`
 * type — project-level code style lives in `.editorconfig` (Phase 1.5);
 * everything moon-ide stores about a user goes here.
 */
export type AppState = {
	last_session: WorkspaceSession | null;
	theme: ThemeMode;
	slack: SlackAppState;
	bottom_panel: BottomPanelAppState;
};

/** Bottom-panel chrome state. Tabs/log streams are intentionally
 * not persisted — they're tied to running compose log processes
 * that don't survive a launch. Mirrors
 * `moon_protocol::app_state::BottomPanelAppState`. */
export type BottomPanelAppState = {
	visible: boolean;
	height: number;
};

/** One line of streamed `docker compose logs` output. Mirrors
 * `moon_protocol::container::LogStreamLine`. */
export type LogStreamLine = {
	stream_id: string;
	channel: string;
	text: string;
};

/** Final event for a log stream when its child process exits.
 * Mirrors `moon_protocol::container::LogStreamClosed`. */
export type LogStreamClosed = {
	stream_id: string;
	code: number | null;
};

/**
 * Where a terminal's shell process runs. Picked at open time
 * and immutable for the tab's life. Mirrors
 * `moon_protocol::terminal::TerminalTarget`.
 *
 * - `host`: the user's machine. `cwd` is an absolute host
 *   path; `null` falls back to `$HOME`.
 * - `container`: the workspace container (`moon-ws-<id>-dev-1`).
 *   `cwd` is a path inside the container — the frontend
 *   computes `/workspace/<basename>` for the active folder
 *   before dispatching the open call.
 */
export type TerminalTarget =
	| { kind: 'host'; cwd: string | null }
	| { kind: 'container'; workspace_id: string; cwd: string };

/** Open-call payload. Mirrors
 * `moon_protocol::terminal::TerminalOpenRequest`. */
export type TerminalOpenRequest = {
	target: TerminalTarget;
	cols: number;
	rows: number;
};

/** One chunk of terminal output. Bytes are base64-encoded —
 * decode with `atob` before feeding xterm.js's `write`.
 * Mirrors `moon_protocol::terminal::TerminalOutput`. */
export type TerminalOutput = {
	stream_id: string;
	data: string;
};

/** Final event for a terminal session when its child exits.
 * Mirrors `moon_protocol::terminal::TerminalClosed`. */
export type TerminalClosed = {
	stream_id: string;
	code: number | null;
};

export const defaultAppState: AppState = {
	last_session: null,
	theme: 'dark',
	slack: { active_bot: null, panel_visible: false, active_thread_ts: null },
	bottom_panel: { visible: false, height: 240 },
};

/**
 * Result of `auth.test`. Identifies the human whose token we hold.
 * Mirrors `moon_protocol::slack::SlackIdentity`.
 */
export type SlackIdentity = {
	user_id: string;
	user_name: string;
	team_id: string;
	team: string;
	url: string;
};

/**
 * A bot we can DM, discovered by scanning the user's own DM list (see
 * `specs/slack-chat.md#bot-resolution`). Mirrors
 * `moon_protocol::slack::SlackBotProfile`.
 */
export type SlackBotProfile = {
	user_id: string;
	dm_channel_id: string;
	username: string;
	real_name: string;
	display_name: string | null;
	image_url: string | null;
};

/**
 * Lightweight connection probe for the chat panel. Mirrors
 * `moon_protocol::slack::SlackStatus`.
 */
export type SlackStatus = {
	connected: boolean;
	identity: SlackIdentity | null;
};

/**
 * One row in the chat panel's session list — a top-level DM message
 * with (or capable of having) a thread under it. Mirrors
 * `moon_protocol::slack::SlackSession`.
 */
export type SlackSession = {
	thread_ts: string;
	latest_ts: string;
	preview: string;
	reply_count: number;
	user_id: string | null;
};

/**
 * One message inside a thread. Mirrors
 * `moon_protocol::slack::SlackMessage`.
 */
export type SlackMessage = {
	ts: string;
	user_id: string | null;
	text: string;
	edited_ts: string | null;
	is_bot: boolean;
	actions: SlackAction[];
	reactions: SlackReaction[];
};

/**
 * One link button extracted from an `actions` block at the bottom of
 * a message (moon-bot's "Response" / "Download" / "Session" footer).
 * Mirrors `moon_protocol::slack::SlackAction`.
 */
export type SlackAction = {
	label: string;
	url: string;
	style: string | null;
};

/**
 * One reaction group on a message. Mirrors
 * `moon_protocol::slack::SlackReaction`. `name` is the Slack
 * shortcode without colons (e.g. `"thumbsup"`); the renderer feeds
 * it through `slackEmoji.emojify` to get a Unicode glyph and falls
 * back to `:name:` for custom workspace emoji we can't resolve.
 */
export type SlackReaction = {
	name: string;
	count: number;
};

/**
 * Trimmed user record used to render `<@U…>` mentions. Mirrors
 * `moon_protocol::slack::SlackUserSummary`. Cached per-user on the
 * frontend to avoid re-hitting `users.info` on every render — see
 * `userCache` in `slack.svelte.ts`.
 */
export type SlackUserSummary = {
	user_id: string;
	name: string;
	real_name: string;
	display_name: string | null;
	is_bot: boolean;
};

/**
 * Best human-readable label for a `users.info` summary. Same fallback
 * chain as [`botLabel`]: `display_name → real_name → username`.
 * Returned without the `@` prefix; rendering decides whether to add
 * one (mention pills do, message authorship lines don't).
 */
export function userLabel(user: SlackUserSummary): string {
	if (user.display_name && user.display_name.length > 0) {
		return user.display_name;
	}
	if (user.real_name.length > 0) {
		return user.real_name;
	}
	return user.name || user.user_id;
}

/**
 * Best human-readable label for a bot profile. Falls back through
 * `display_name → real_name → username` so the panel always shows
 * *something* even when Slack returns sparse metadata.
 */
export function botLabel(profile: SlackBotProfile): string {
	if (profile.display_name && profile.display_name.length > 0) {
		return profile.display_name;
	}
	if (profile.real_name.length > 0) {
		return profile.real_name;
	}
	return profile.username || profile.user_id;
}

/**
 * High-level state of the workspace's compose project. Mirrors
 * `moon_protocol::container::ContainerState`. See
 * `crates/moon-container/src/lifecycle.rs#aggregate_state` for
 * the precedence rules behind each variant.
 */
export type ContainerState = 'absent' | 'creating' | 'running' | 'paused' | 'stopped' | 'failed';

/**
 * One container in the compose project, as reported by
 * `docker compose ps --format json`. Mirrors
 * `moon_protocol::container::ServiceStatus`.
 */
export type ServiceStatus = {
	name: string;
	/** Raw Docker container state (`running`, `paused`, `exited`, `created`, `restarting`, `dead`). */
	raw_state: string;
	/** Process exit code. Compose emits `0` for non-exited states too — only meaningful when `raw_state === 'exited'`. */
	exit_code: number;
	/** Healthcheck verdict (`healthy`, `unhealthy`, `starting`); empty string when no healthcheck declared. */
	health: string;
};

/**
 * `true` for the conventional "process was terminated by a stop
 * signal" exit codes — `130` (SIGINT), `137` (SIGKILL), `143`
 * (SIGTERM). These are what `docker compose stop` (and the IDE's
 * shutdown hook) produce; they are *not* application failures, so
 * the per-service indicator stays muted instead of going red.
 *
 * Mirrors `is_stop_signal` in
 * `crates/moon-container/src/lifecycle.rs` — keep the two in sync.
 * SIGSEGV (139), SIGABRT (134), SIGBUS (135), and friends are
 * deliberately *not* on this list: those are real crashes the
 * user should see surfaced.
 */
export function isStopSignal(exitCode: number): boolean {
	return exitCode === 130 || exitCode === 137 || exitCode === 143;
}

/**
 * `true` when a service row should be rendered as "this is broken
 * and won't recover on its own" (solid red dot, no pulse). Plain
 * `exited (0)` and signal-terminated exits stay muted.
 */
export function isFailedService(svc: ServiceStatus): boolean {
	if (svc.raw_state === 'exited' && svc.exit_code !== 0 && !isStopSignal(svc.exit_code)) {
		return true;
	}
	if (svc.raw_state === 'dead') {
		return true;
	}
	if (svc.raw_state === 'running' && svc.health === 'unhealthy') {
		return true;
	}
	return false;
}

/**
 * Snapshot returned by `container_status` and embedded in every
 * `container:state` event. Mirrors
 * `moon_protocol::container::ContainerStatus`.
 */
export type ContainerStatus = {
	state: ContainerState;
	services: ServiceStatus[];
};

/**
 * Payload of the `container:state` Tauri event. Includes
 * `workspace_id` so once multi-window arrives the right pip
 * updates; in 2.0 it always matches the active workspace.
 * Mirrors `moon_protocol::container::ContainerStateChange`.
 */
export type ContainerStateChange = {
	workspace_id: string;
	status: ContainerStatus;
};

/**
 * Status of one bound folder's compose project (its own
 * `docker-compose.yml`). The folder bar's compose indicator
 * reads this; `compose_file == null` means the folder has no
 * compose file at its root and the indicator stays hidden.
 * Mirrors `moon_protocol::container::ProjectComposeStatus`.
 */
export type ProjectComposeStatus = {
	folder_path: string;
	compose_file: string | null;
	project_name: string | null;
	status: ContainerStatus;
};

/**
 * Payload of the `project_compose:state` Tauri event,
 * broadcast after every per-folder lifecycle command. The
 * `folder_path` field is the routing key — the UI updates only
 * the matching folder bar without re-querying the others.
 * Mirrors `moon_protocol::container::ProjectComposeStateChange`.
 */
export type ProjectComposeStateChange = {
	workspace_id: string;
	folder_path: string;
	project: ProjectComposeStatus;
};

export type MoonError =
	| { code: 'NotFound'; message: string }
	| { code: 'IoError'; message: string }
	| { code: 'PermissionDenied'; message: string }
	| { code: 'HostUnavailable'; message: string }
	| { code: 'InvalidArgument'; message: string }
	| { code: 'Internal'; message: string };

export function isMoonError(err: unknown): err is MoonError {
	return (
		typeof err === 'object' &&
		err !== null &&
		'code' in err &&
		'message' in err &&
		typeof (err as { code: unknown }).code === 'string'
	);
}

export function formatError(err: unknown): string {
	if (isMoonError(err)) {
		return `${err.code}: ${err.message}`;
	}
	if (err instanceof Error) {
		return err.message;
	}
	return String(err);
}
