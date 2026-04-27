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

export type Workspace = {
	id: string;
	name: string;
	root: string;
	host: HostKind;
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
 * Persisted UI session. Frontend-owned shape; the backend is pure
 * storage. Workspace-relative paths (relative to `workspace_path`).
 * The two `open_files_*` lists are independent — a path can live in
 * one pane, both, or neither (VSCode/Zed convention).
 */
export type WorkspaceSession = {
	workspace_path: string;
	open_files_left: string[];
	open_files_right: string[];
	active_left: string | null;
	active_right: string | null;
	has_split: boolean;
	focused_side: SplitSide;
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
};

export const defaultAppState: AppState = {
	last_session: null,
	theme: 'dark',
	slack: { active_bot: null, panel_visible: false, active_thread_ts: null },
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
