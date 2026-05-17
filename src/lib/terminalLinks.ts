//! Stack-trace / file-link detection for terminal panes.
//!
//! Scans a row of terminal text for path-shaped matches the user
//! might want to Ctrl-click to open in the editor. Two flavours
//! land in scope:
//!
//! 1. `file:///abs/path` URIs (with optional `:line[:col]` tail).
//! 2. Bare absolute paths starting with `/` (same `:line[:col]`
//!    tail).
//!
//! Both forms support the host shape (`/home/me/code/...`) and
//! the container shape (`/workspace/<basename>/...`) — the latter
//! is reverse-mapped through the bound-folder list using the
//! same `containerCwdFor` rule the terminal opens with.
//!
//! Relative paths (`src/lib/foo.ts:22`) are out of scope: the
//! terminal only knows its *opening* cwd, and once the user
//! `cd`s the relationship is gone without shell integration.
//! Relative-path support can be added later if a real need shows
//! up; for now they're left as plain text so the user can copy
//! them by hand.
//!
//! Pure module, no Svelte / no IPC. The terminal component glues
//! it to xterm's `registerLinkProvider` and to
//! [`workspace.jumpTo`].

import type { Workspace } from './protocol';

/** One file-link match inside a row of terminal text. Coords are
 *  0-based JS string indices into the source line — the caller
 *  converts to xterm's 1-based inclusive `IBufferRange` shape. */
export type ParsedFileLink = {
	/** Inclusive start, 0-based. */
	start: number;
	/** Exclusive end, 0-based. */
	end: number;
	/** Path as written in the trace, with any `file://` prefix
	 *  stripped. Still in source-encoding (i.e. percent-encoded
	 *  for URI matches); decoders sit at the resolution
	 *  boundary. */
	rawPath: string;
	/** Line number as written in the trace, 1-based. `null` when
	 *  the path had no `:line` tail. */
	line: number | null;
	/** Column number as written, 1-based. `null` when no
	 *  `:line:col` tail or only `:line` was present. */
	col: number | null;
};

/** Resolved navigation target the caller can hand straight to
 *  `workspace.jumpTo(path, position, side, folder)`. Coords are
 *  **0-based** (CodeMirror / LSP convention) — the parser's
 *  1-based line/col have been decremented. */
export type ResolvedTerminalLink = {
	/** Absolute host path of the bound folder the link resolves
	 *  to. `jumpTo` treats this as the folder argument. */
	folder: string;
	/** Path relative to `folder`. Same shape `openFile` and
	 *  `resolveExternalUri` already speak. */
	path: string;
	/** 0-based caret position. Defaults to `(0, 0)` when the
	 *  trace didn't carry a `:line[:col]` tail. */
	line: number;
	character: number;
};

// Path body: anything that isn't whitespace, a separator we use
// for our suffix grammar (`:`), bracket-style decoration (`()[]<>`),
// quotes, comma, or backtick. This is wide enough to allow `-`,
// `.`, `_`, dotted segments, and unicode letters; narrow enough
// to keep a sentence like "see /etc/foo, then bar" from greedy-
// matching across the comma. Multi-line scans never cross a
// newline because xterm hands us one row at a time, but we
// exclude `\s` defensively.
const PATH_BODY_CHAR = '[^\\s:()\\[\\]<>\'",`]';
const PATH_BODY = `${PATH_BODY_CHAR}+`;

// Negative lookbehind: refuse to start a match in the middle of
// what the eye reads as a single token. Without it, a relative
// path like `src/lib/foo.ts:22:3` would get the `/lib/foo.ts:22:3`
// suffix matched as if it were absolute. Anchoring on a
// path-boundary char (whitespace / paren / bracket / quote / start
// of row) is what every terminal-link addon does to make this
// honest.
const NOT_PATH_BODY = `(?<!${PATH_BODY_CHAR})`;

// Single regex covering both flavours. The optional `file://`
// is matched cheaply at the start; the absolute path body
// follows. The two trailing `:N` captures are independent so
// we accept `:line` alone, `:line:col`, or no tail. `g` flag
// to scan an entire row.
const FILE_LINK_RE = new RegExp(`${NOT_PATH_BODY}(?:file:\\/\\/)?(\\/${PATH_BODY})(?::(\\d+)(?::(\\d+))?)?`, 'g');

/** Scan a row's text for absolute paths (`/...`) or `file://`
 *  URIs, with optional `:line[:col]` tails. Returns matches in
 *  source order. Relative paths are intentionally skipped. */
export function parseFileLinks(text: string): ParsedFileLink[] {
	const out: ParsedFileLink[] = [];
	// `lastIndex` state on the shared regex would corrupt
	// across calls; clone the pattern per scan instead — a few
	// dozen rows per second tops, allocation cost is invisible
	// next to xterm's redraw.
	const re = new RegExp(FILE_LINK_RE.source, FILE_LINK_RE.flags);
	let m: RegExpExecArray | null = re.exec(text);
	while (m !== null) {
		const matched = m[0];
		const path = m[1];
		// Skip if the regex's optional `file://` branch ate
		// nothing and `path` somehow came back empty — defensive
		// against pathological inputs; the path body requires `+`
		// internally so this should never fire.
		if (path === undefined || path.length === 0) {
			m = re.exec(text);
			continue;
		}
		const lineStr = m[2];
		const colStr = m[3];
		const line = lineStr === undefined ? null : Number(lineStr);
		const col = colStr === undefined ? null : Number(colStr);

		// Strip a single trailing punctuation char that's
		// regex-legal but rarely part of a real path: a sentence
		// like "see /etc/foo." or "the patch /tmp/x.diff." would
		// otherwise leave a `.` in the link. We only do this
		// when the path has no `:line` tail (the `:NN` form
		// already protects against this — `foo.:22` doesn't
		// happen). `;` and `?` and `!` likewise.
		let trimmedPath = path;
		let trimmedMatch = matched;
		if (line === null) {
			const last = trimmedPath.charCodeAt(trimmedPath.length - 1);
			// `.` `,` `;` `?` `!` `)` `]`
			if (last === 0x2e || last === 0x2c || last === 0x3b || last === 0x3f || last === 0x21) {
				trimmedPath = trimmedPath.slice(0, -1);
				trimmedMatch = trimmedMatch.slice(0, -1);
			}
		}

		// Must look path-shaped: either carry a `:line` tail
		// (proves it's a source pointer, not an arbitrary
		// directory), or end in something extension-shaped on
		// the last segment. Without this, `cat /etc/passwd`
		// output would be flecked with link decorations against
		// every absolute path the shell printed.
		if (line === null && !lastSegmentLooksLikeFile(trimmedPath)) {
			m = re.exec(text);
			continue;
		}

		const start = m.index;
		const end = start + trimmedMatch.length;
		out.push({ start, end, rawPath: trimmedPath, line, col });
		m = re.exec(text);
	}
	return out;
}

/** Heuristic for "is this absolute path the kind of thing one
 *  would open in a text editor?" — true when the last path
 *  segment carries a recognisable file extension or matches a
 *  conventional extensionless source name. Used to suppress
 *  links on generic directory mentions (`/etc`, `/home/me`)
 *  when there's no `:line` tail to disambiguate. */
function lastSegmentLooksLikeFile(path: string): boolean {
	const slash = path.lastIndexOf('/');
	const seg = slash === -1 ? path : path.slice(slash + 1);
	if (seg.length === 0) {
		return false;
	}
	const dot = seg.lastIndexOf('.');
	if (dot > 0 && dot < seg.length - 1) {
		const ext = seg.slice(dot + 1);
		// Reasonable max for a real file extension; longer
		// "extensions" are almost always part of a hash
		// (`.0123abcdef…`) we don't want to link.
		return ext.length > 0 && ext.length <= 12 && /^[A-Za-z0-9]+$/.test(ext);
	}
	// Common extensionless sources the team actually edits.
	// Conservative on purpose; anything outside the list still
	// links when accompanied by a `:line` tail.
	return EXTENSIONLESS_FILE_NAMES.has(seg);
}

const EXTENSIONLESS_FILE_NAMES = new Set([
	'Cargo.lock',
	'Dockerfile',
	'Makefile',
	'Procfile',
	'Rakefile',
	'Gemfile',
	'Pipfile',
	'Vagrantfile',
	'README',
	'LICENSE',
	'CHANGELOG',
]);

/** Resolve a parsed link against the workspace's bound folders.
 *  Returns the cross-folder navigation tuple a `jumpTo` call
 *  expects, or `null` when the path falls outside every bound
 *  folder.
 *
 *  Resolution order:
 *  1. Treat the path as a host absolute (`/home/me/...`); walk
 *     bound folders longest-prefix-first.
 *  2. Treat the path as a container `/workspace/<basename>/...`
 *     mount; reverse the basename mapping against bound
 *     folders. First basename match wins, mirroring the
 *     forward direction in `containerCwdFor`.
 *
 *  Both branches run regardless of which terminal target the
 *  link came from — a host shell that `cat`s a container log
 *  shouldn't lose its links, and vice versa. */
export function resolveTerminalLink(parsed: ParsedFileLink, workspace: Workspace | null): ResolvedTerminalLink | null {
	if (workspace === null || workspace.folders.length === 0) {
		return null;
	}
	let abs: string;
	try {
		// Source-encoding may be percent-escaped (`%20` in spaces);
		// `decodeURIComponent` covers both flavours since bare
		// absolute paths are rarely escaped to begin with.
		abs = decodeURIComponent(parsed.rawPath);
	} catch {
		abs = parsed.rawPath;
	}

	const hostHit = matchHostPath(abs, workspace);
	if (hostHit !== null) {
		return finishResolution(hostHit, parsed);
	}
	const containerHit = matchContainerPath(abs, workspace);
	if (containerHit !== null) {
		return finishResolution(containerHit, parsed);
	}
	return null;
}

function matchHostPath(abs: string, workspace: Workspace): { folder: string; path: string } | null {
	const sorted = workspace.folders.toSorted((a, b) => b.path.length - a.path.length);
	for (const folder of sorted) {
		const root = folder.path.endsWith('/') ? folder.path : `${folder.path}/`;
		if (abs === folder.path) {
			return { folder: folder.path, path: '' };
		}
		if (abs.startsWith(root)) {
			return { folder: folder.path, path: abs.slice(root.length) };
		}
	}
	return null;
}

function matchContainerPath(abs: string, workspace: Workspace): { folder: string; path: string } | null {
	const WORKSPACE_PREFIX = '/workspace/';
	if (!abs.startsWith(WORKSPACE_PREFIX)) {
		return null;
	}
	const tail = abs.slice(WORKSPACE_PREFIX.length);
	const slash = tail.indexOf('/');
	const basename = slash === -1 ? tail : tail.slice(0, slash);
	if (basename.length === 0) {
		return null;
	}
	const relative = slash === -1 ? '' : tail.slice(slash + 1);
	for (const folder of workspace.folders) {
		const folderBase = basenameOf(folder.path);
		if (folderBase === basename) {
			return { folder: folder.path, path: relative };
		}
	}
	return null;
}

function basenameOf(path: string): string {
	const trimmed = path.replace(/\/+$/, '');
	const slash = trimmed.lastIndexOf('/');
	return slash === -1 ? trimmed : trimmed.slice(slash + 1);
}

function finishResolution(hit: { folder: string; path: string }, parsed: ParsedFileLink): ResolvedTerminalLink {
	// The trace's line/col are 1-based by convention (every
	// compiler / runtime emits them that way). `jumpTo`
	// speaks 0-based, so subtract — clamped at 0 in case an
	// overzealous trace prints `:0` (which a 1-based emitter
	// shouldn't, but defensive doesn't cost anything).
	const line = parsed.line === null ? 0 : Math.max(0, parsed.line - 1);
	const character = parsed.col === null ? 0 : Math.max(0, parsed.col - 1);
	return { folder: hit.folder, path: hit.path, line, character };
}
