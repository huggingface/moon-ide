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

/**
 * Persisted UI session. Frontend-owned shape; the backend is pure
 * storage. Workspace-relative paths (relative to `workspace_path`).
 */
export type WorkspaceSession = {
	workspace_path: string;
	open_files: string[];
	active_left: string | null;
	active_right: string | null;
	has_split: boolean;
	focused_side: SplitSide;
};

/**
 * Per-machine, per-user app state. There is intentionally no `Settings`
 * type — project-level code style lives in `.editorconfig` (Phase 1.5);
 * everything moon-ide stores about a user goes here.
 */
export type AppState = {
	last_session: WorkspaceSession | null;
	theme: ThemeMode;
};

export const defaultAppState: AppState = {
	last_session: null,
	theme: 'dark',
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
