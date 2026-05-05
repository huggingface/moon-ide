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

/**
 * What the user picked in the theme switcher. `'system'` means
 * "follow the OS" and gets resolved to dark/light at render time
 * on the frontend — see `WorkspaceState.effectiveTheme`. Mirrors
 * `moon_protocol::theme::ThemeMode`.
 */
export type ThemeMode = 'system' | 'dark' | 'light';

/**
 * Resolved OS colour-scheme preference from the desktop shell.
 * `'unspecified'` maps to the XDG portal's "no preference" value,
 * which we treat as dark (moon-ide defaults to dark chrome).
 * Mirrors `moon_protocol::theme::SystemTheme`.
 */
export type SystemTheme = 'dark' | 'light' | 'unspecified';

/**
 * One path's git status. The vocabulary matches Pierre Trees' own
 * `GitStatus` type so frontend code can pass `GitStatusEntry[]`
 * straight through to `tree.setGitStatus`. Mirrors
 * `moon_protocol::git::GitFileStatus`.
 */
export type GitFileStatus = 'added' | 'modified' | 'deleted' | 'untracked' | 'ignored';

/**
 * One row's git classification. `path` follows the usual trailing-
 * slash convention for directories; `deleted` rows never carry one
 * (git tracks files, not dirs, in this model). Mirrors
 * `moon_protocol::git::GitStatusEntry`.
 */
export type GitStatusEntry = {
	path: string;
	status: GitFileStatus;
};

/**
 * Per-line blame for the inline current-line annotation and its
 * hover tooltip. Mirrors `moon_protocol::git::GitLineBlame`. The
 * `isUncommitted` flag is a convenience peel-off of the all-zero
 * sha sentinel git emits for local edits; frontend code shouldn't
 * need to know the sentinel string.
 */
export type GitLineBlame = {
	sha: string;
	isUncommitted: boolean;
	author: string;
	authorEmail: string;
	/** Unix timestamp in seconds (UTC). */
	authorTime: number;
	summary: string;
	message: string;
};

/**
 * Per-file blame report, one entry per source line, 0-indexed to
 * match CodeMirror's line addressing after the `line(n + 1)`
 * adjustment. Mirrors `moon_protocol::git::GitFileBlame`.
 *
 * `path` is echoed back so a late-arriving response (the user
 * switched files while a blame subprocess was still running) can be
 * discarded at the call site without leaking stale annotations.
 */
export type GitFileBlame = {
	path: string;
	/**
	 * Canonical HTTPS base URL of the repo's primary remote when it's
	 * a host we know how to build PR / issue links for (currently
	 * `github.com` only). Empty string means "no link target" — the
	 * frontend falls back to rendering `#NNN` as plain text.
	 */
	remoteUrl: string;
	lines: GitLineBlame[];
};

/**
 * LSP diagnostic severity. Mirrors `moon_protocol::lsp::LspSeverity`.
 * The four-level gradient matches LSP's own enum; the UI maps each
 * level to an icon + gutter colour.
 */
export type LspSeverity = 'error' | 'warning' | 'info' | 'hint';

/**
 * LSP position (zero-based line + UTF-16 character offset). Same
 * encoding CodeMirror uses natively for `Line` + `col`; we pass
 * values through both directions without conversion. Mirrors
 * `moon_protocol::lsp::LspPosition`.
 */
export type LspPosition = {
	line: number;
	character: number;
};

export type LspRange = {
	start: LspPosition;
	end: LspPosition;
};

/**
 * One diagnostic from a language server. `source` and `code` are
 * surfaced in the tooltip so a user can tell which producer emitted
 * the warning (e.g. `"ts"` vs `"eslint"`). Mirrors
 * `moon_protocol::lsp::LspDiagnostic`.
 */
export type LspDiagnostic = {
	range: LspRange;
	severity: LspSeverity;
	message: string;
	source: string | null;
	code: string | null;
};

/**
 * Event payload delivered on `lsp:diagnostics`. Full replacement
 * semantics: the list is the server's new truth for `path`, so the
 * UI overwrites instead of merging. Mirrors
 * `moon_protocol::lsp::LspDiagnosticsEvent`.
 */
export type LspDiagnosticsEvent = {
	path: string;
	diagnostics: LspDiagnostic[];
};

/**
 * Normalised hover response: Markdown body + optional range. Empty
 * hovers are coalesced to `null` on the backend so the UI never
 * opens a blank tooltip. Mirrors `moon_protocol::lsp::LspHover`.
 */
export type LspHover = {
	contents: string;
	range: LspRange | null;
};

/**
 * Definition jump target. Exactly one of `path` / `externalUri` is
 * non-empty — in-workspace targets use `path`, external targets
 * (node_modules, toolchain sources) use `externalUri`. Mirrors
 * `moon_protocol::lsp::LspLocation`.
 */
export type LspLocation = {
	path: string;
	range: LspRange;
	externalUri: string;
};

/**
 * Kind of a completion item. Mirrors LSP's list 1:1; the frontend
 * uses it for iconography. Extending this set requires adding to
 * `moon_protocol::lsp::LspCompletionKind` and the `translate` match.
 */
export type LspCompletionKind =
	| 'text'
	| 'method'
	| 'function'
	| 'constructor'
	| 'field'
	| 'variable'
	| 'class'
	| 'interface'
	| 'module'
	| 'property'
	| 'unit'
	| 'value'
	| 'enum'
	| 'keyword'
	| 'snippet'
	| 'color'
	| 'file'
	| 'reference'
	| 'folder'
	| 'enummember'
	| 'constant'
	| 'struct'
	| 'event'
	| 'operator'
	| 'typeparameter';

export type LspCompletionItem = {
	label: string;
	kind: LspCompletionKind | null;
	detail: string | null;
	documentation: string | null;
	insertText: string | null;
	sortText: string | null;
	filterText: string | null;
};

export type LspCompletionList = {
	isIncomplete: boolean;
	items: LspCompletionItem[];
};

/**
 * Per-language server state. Emitted on `lsp:status` whenever the
 * broker transitions a server between states. UI caches the latest
 * per language id and paints a status-bar pill when it's anything
 * but `running`. Mirrors `moon_protocol::lsp::LspServerStatus`.
 */
export type LspServerStatus = 'notavailable' | 'starting' | 'running' | 'crashed' | 'stopped';

export type LspStatusEvent = {
	languageId: string;
	status: LspServerStatus;
	detail: string | null;
};

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
 *
 * Right-panel visibility lives on [`AppState.right_panel`] now (chat
 * and coder share one slot); this slice no longer carries it.
 */
export type SlackAppState = {
	active_bot: SlackBotProfile | null;
	active_thread_ts: string | null;
};

/**
 * Surface mounted in the right-side panel. Chat and coder are
 * mutually exclusive: opening one swaps the other out. The slot can
 * also be closed entirely (`null` on `AppState.right_panel`).
 * Mirrors `moon_protocol::app_state::RightPanelKind`.
 */
export type RightPanelKind = 'chat' | 'coder';

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
	right_panel: RightPanelKind | null;
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
	theme: 'system',
	slack: { active_bot: null, active_thread_ts: null },
	bottom_panel: { visible: false, height: 240 },
	right_panel: null,
};

/**
 * Identifies the human whose token we hold, plus enough chrome
 * (workspace icon) for the chat-panel header. Mirrors
 * `moon_protocol::slack::SlackIdentity`.
 */
export type SlackIdentity = {
	user_id: string;
	user_name: string;
	team_id: string;
	team: string;
	url: string;
	icon_url: string | null;
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

/**
 * Hugging Face user identity returned by `coder_status` and the
 * device-flow completion. Mirrors `moon_coder::auth::HfIdentity`.
 */
export type HfIdentity = {
	username: string;
	name: string | null;
	avatar_url: string | null;
	email: string | null;
};

/**
 * Device-code response from `coder_start_device_flow`. The frontend
 * shows `user_code`, opens `verification_uri_complete` (falling back
 * to `verification_uri`) in the system browser, then awaits
 * `coder_poll_device_code`. Mirrors `moon_coder::auth::DeviceCode`.
 */
export type DeviceCode = {
	user_code: string;
	verification_uri: string;
	verification_uri_complete: string | null;
	expires_in: number;
	interval: number;
	device_code: string;
};

/** Snapshot returned by `coder_status`. Mirrors `moon_coder::CoderStatus`. */
export type CoderStatus = {
	signed_in: boolean;
	identity: HfIdentity | null;
	busy: boolean;
	/**
	 * Where the agent's `bash` tool runs for the active folder. Mirrors
	 * the `target` field on the bash tool result. `null` when the
	 * workspace has no active folder yet.
	 */
	bash_target: 'host' | 'container' | null;
};

/**
 * Tagged-union of agent-loop events emitted on the `coder:event`
 * Tauri channel. Mirrors `moon_coder::CoderEvent`. The frontend
 * builds its message list from the running stream — no REST replay,
 * because 6.0 doesn't persist the session.
 */
export type CoderEvent =
	| { kind: 'user_message'; id: string; text: string }
	| { kind: 'assistant_message_start'; id: string }
	| { kind: 'assistant_message_delta'; id: string; delta: string }
	| { kind: 'assistant_thinking_delta'; id: string; delta: string }
	| { kind: 'assistant_message_end'; id: string; text: string; thinking?: string | null }
	| { kind: 'tool_call'; id: string; name: string; args: unknown }
	| { kind: 'tool_result'; id: string; result: unknown; is_error: boolean }
	| { kind: 'turn_complete' }
	| { kind: 'aborted' }
	| { kind: 'error'; message: string };

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
