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

/**
 * Mirror of `moon_protocol::fs::CollectPathsResult` — the
 * file-tree path enumeration plus the directories whose recursion
 * the depth cap stopped short of so the frontend can lazy-load
 * them on expansion.
 */
export type CollectPathsResult = {
	paths: string[];
	depth_capped: string[];
};

export type HostKind = 'local' | 'devcontainer';

/**
 * Why a folder is bound into the workspace. `userPicked` is the
 * default ("Open folder…"); `worktree` is an IDE-managed git
 * worktree backing an isolated coder session (ADR 0028), rendered
 * nested under `parentPath` with the `branch` label. Mirrors
 * `moon_protocol::workspace::FolderOrigin`.
 */
export type FolderOrigin = { kind: 'user_picked' } | { kind: 'worktree'; parentPath: string; branch: string };

/**
 * One folder bound into a workspace. Mirrors
 * `moon_protocol::workspace::WorkspaceFolder`.
 */
export type WorkspaceFolder = {
	path: string;
	name: string;
	host: HostKind;
	origin: FolderOrigin;
};

/**
 * Workspace identifier — a slug like `"huggingface"` /
 * `"gitaly"`. Process-per-workspace: each `moon-ide` process
 * owns one workspace named at startup via `--workspace <id>`;
 * the slug is used for the per-workspace state directory
 * (`<workspaces_dir>/<id>/`), the compose project name
 * (`moon-ws-<id>`), and the per-workspace single-instance
 * lock socket. Mirrors `moon_protocol::workspace::WorkspaceId`.
 */
export type WorkspaceId = string;

/**
 * Process mode reported by the backend's `app_info` IPC.
 * `workspace` means a real workspace was mounted at startup
 * (CLI arg or auto-restored from the catalog); `preboot`
 * means the catalog was empty and the user hasn't named a
 * workspace yet. Mirrors `moon_protocol::app_info::AppInfoMode`.
 */
export type AppInfoMode = 'workspace' | 'preboot';

/**
 * Bootstrap information the frontend reads exactly once on
 * hydrate. The values never change for a process's lifetime.
 * Mirrors `moon_protocol::app_info::AppInfo`.
 */
export type AppInfo = {
	mode: AppInfoMode;
	workspaceId: WorkspaceId | null;
	workspaceName: string | null;
};

/**
 * The full workspace shape — the running process's single
 * workspace, holding zero or more folders with at most one
 * currently active. Mirrors `moon_protocol::workspace::Workspace`.
 */
export type Workspace = {
	id: string;
	folders: WorkspaceFolder[];
	active_folder: string | null;
};

/**
 * Catalog entry for a workspace the user has on this machine.
 * Mirrors `moon_protocol::workspace::WorkspaceMeta`. Lives in
 * `AppState.workspaces`; distinct from the live `Workspace`
 * snapshot above (which carries folders + active for one
 * workspace).
 */
export type WorkspaceMeta = {
	/** Slug id; passes `[a-z0-9_-]` so `moon-ws-<id>` is valid. */
	id: string;
	/** Display name (free text). */
	name: string;
	/** Unix epoch seconds; bumped on every launch. */
	last_active_at: number;
	/** User-chosen badge colour as `#rrggbb`. `null` / absent
	 * means "use the deterministic hash-derived hue" — the
	 * default every new workspace starts with. Drives the
	 * per-window icon Tauri sets at startup. */
	color?: string | null;
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
	/** Only match at word boundaries. Stacks with `regex`. */
	whole_word?: boolean;
	/** Restrict the walk to paths matching this gitignore-style glob.
	 *  Empty / undefined means "search everything". A bare path (`src/lib`)
	 *  is normalised to `src/lib/**` server-side, so users don't need to
	 *  know glob syntax for the common "scope to subdirectory" case. */
	include_glob?: string | null;
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

export type ContentReplaceOptions = {
	query: string;
	replacement: string;
	case_sensitive?: boolean;
	regex?: boolean;
	whole_word?: boolean;
	include_glob?: string | null;
};

export type ContentReplaceResult = {
	files_changed: number;
	replacements: number;
	errors: ContentReplaceError[];
};

export type ContentReplaceError = {
	path: string;
	message: string;
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
 * One path's git status. Five of the six tokens (`added`, `modified`,
 * `deleted`, `untracked`, `ignored`) match Pierre Trees' built-in
 * `GitStatus` so frontend code can pass `GitStatusEntry[]` straight
 * through to `tree.setGitStatus`. The sixth (`conflicted`) is
 * unique to us — Pierre's own enum doesn't carry it — and the
 * `FileTree` component overlays a separate "!" badge for rows that
 * report it. Mirrors `moon_protocol::git::GitFileStatus`.
 */
export type GitFileStatus = 'added' | 'modified' | 'deleted' | 'untracked' | 'ignored' | 'conflicted';

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
 * Aggregate change counts for a single bound folder, used to paint
 * the per-folder badges on the project bar. Untracked files fold
 * into `added` because the bar only needs a single "this folder
 * has new files" signal — the SCM panel inside the active folder
 * still distinguishes them. Mirrors
 * `moon_protocol::git::GitChangeSummary`.
 */
export type GitChangeSummary = {
	added: number;
	modified: number;
	deleted: number;
};

/**
 * Snapshot of an in-flight merge for the SCM panel. The panel
 * reshapes itself (header pill, "Commit merge" / "Abort merge"
 * buttons, hidden sync controls) when `inProgress` is `true`.
 * Mirrors `moon_protocol::git::GitMergeState`. See the Rust doc
 * for the field-by-field contract.
 */
export type GitMergeState = {
	inProgress: boolean;
	mergingRef: string | null;
	defaultMessage: string | null;
	unmergedPaths: string[];
};

/**
 * Which side of the diff a review comment is anchored to —
 * `'working'` (GitHub `RIGHT`, the code as it will land, the common
 * case) or `'base'` (GitHub `LEFT`, the deleted / old side). Mirrors
 * `moon_protocol::review::ReviewSide`.
 */
export type ReviewSide = 'base' | 'working';

/**
 * Where a [`ReviewComment`] points. `startLine` / `endLine` are a
 * fast-path render hint; `fingerprint` is the source of truth used
 * to re-locate the anchor after the text drifts. Mirrors
 * `moon_protocol::review::ReviewAnchor`.
 */
export type ReviewAnchor = {
	path: string;
	side: ReviewSide;
	/** 1-based, in the side's current text — a hint. */
	startLine: number;
	/** 1-based; equals `startLine` for single-line comments. */
	endLine: number;
	/** Hash of the trimmed anchored line text(s). */
	fingerprint: string;
	/** Merge-base / `HEAD` SHA the comment was written against. */
	baselineRev: string;
};

/**
 * One local-first review-comment draft. One author, one body, one
 * anchor — no threading. Lives in the workspace session until
 * published to GitHub and then cleared locally. Mirrors
 * `moon_protocol::review::ReviewComment`.
 */
export type ReviewComment = {
	/** ULID assigned on create. */
	id: string;
	anchor: ReviewAnchor;
	/** Markdown comment text. */
	body: string;
	/** RFC3339 creation timestamp. */
	createdAt: string;
};

/**
 * A per-file "Viewed" mark, pinned to the ticked version's blob
 * SHA. The frontend drops the mark when the file's current blob SHA
 * no longer matches `reviewedRev`. Never published. Mirrors
 * `moon_protocol::review::ReviewedFile`.
 */
export type ReviewedFile = {
	path: string;
	/** Blob SHA (`git hash-object`) of the ticked version. */
	reviewedRev: string;
	/** RFC3339 timestamp of when the file was marked reviewed. */
	reviewedAt: string;
};

/**
 * Request to publish a batch of local comments as one GitHub PR
 * review. Mirrors `moon_protocol::review::PublishReviewRequest`.
 * (The publish path itself lands in Phase 5.7.2.)
 */
export type PublishReviewRequest = {
	body: string | null;
	comments: ReviewComment[];
};

/**
 * Outcome of a publish. `'no_pr'` when the current branch has no
 * open PR (UI shows a create-PR CTA); `'published'` carries how many
 * comments landed, the ids of any that couldn't be placed at the PR
 * head (kept as local drafts), and the posted review URL. Mirrors
 * the tagged enum `moon_protocol::review::PublishReviewResult`.
 */
export type PublishReviewResult =
	| { kind: 'no_pr'; branch: string }
	| { kind: 'published'; posted: number; lost: string[]; reviewUrl: string };

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
/**
 * Branch + HEAD info for the SCM panel header. All-`null` is the
 * "no branch label" fallback (folder isn't a git repo, detached
 * HEAD with unreadable commit, etc.). Mirrors
 * `moon_protocol::git::GitBranchInfo`.
 */
export type GitBranchInfo = {
	name: string | null;
	headShortSha: string | null;
	/**
	 * Whether the current branch has a configured upstream
	 * (`branch.<name>.remote` + `branch.<name>.merge`). `false`
	 * for a freshly-created local branch never pushed, detached
	 * HEAD, non-repo folders, and folders without git available.
	 * Lets the SCM panel pick between the sync button (upstream
	 * exists) and a "Publish branch" affordance (no upstream yet).
	 * Note: a fork-PR upstream (set by `gh pr checkout`, where
	 * `branch.<name>.remote` is a URL rather than a named remote)
	 * still counts as having an upstream; see `upstreamTracked`.
	 */
	hasUpstream: boolean;
	/**
	 * Whether the configured upstream is a tracked named remote
	 * (`@{u}` resolves to a `refs/remotes/...` ref). `false` for
	 * the `gh pr checkout` fork-PR shape where the upstream is a
	 * bare URL; in that state the backend can't compute ahead /
	 * behind without a network call, so `ahead` and `behind` are
	 * always 0 and the SCM panel renders Sync Changes without
	 * count badges so the user can still push back to the fork.
	 * Always `false` when `hasUpstream` is `false`.
	 */
	upstreamTracked: boolean;
	/**
	 * Whether the upstream is a *foreign* tracked branch — a
	 * named-remote branch (`upstreamTracked`) whose name differs
	 * from the local branch. This is the `git checkout -b feature
	 * origin/main` shape: the branch tracks `refs/heads/main` for
	 * pull / rebase but owns no remote branch yet. Pushing to that
	 * upstream would land the feature commits straight on `main`,
	 * so the SCM panel treats it like an unpublished branch and
	 * offers "Publish branch" instead of Sync Changes. `false` for
	 * the normal same-name upstream, the fork-PR URL shape, and
	 * whenever `upstreamTracked` is `false`.
	 */
	upstreamForeign: boolean;
	/** Commits the local branch has that upstream doesn't (push count). 0 when no upstream / no HEAD / untracked upstream. */
	ahead: number;
	/** Commits upstream has that the local branch doesn't (pull count). 0 when no upstream / no HEAD / untracked upstream. */
	behind: number;
	/**
	 * Pre-built URL for opening a PR against the repo's primary
	 * remote (e.g. `https://github.com/owner/repo/pull/new/<branch>`).
	 * `null` when the remote isn't a recognised host (currently
	 * only `github.com` is supported), HEAD is detached, or the
	 * folder isn't a git repo. The SCM panel still gates the
	 * "Open PR" button on UI policy (non-main / non-master,
	 * `hasUpstream`).
	 */
	prUrl: string | null;
	/**
	 * Remote-tracking ref for the repo's default branch, e.g.
	 * `"origin/main"`. Resolved from `refs/remotes/origin/HEAD`
	 * with fallbacks to `origin/main` then `origin/master`.
	 * `null` when no default can be resolved. The SCM panel
	 * passes this verbatim to `git_merge_default_branch` and
	 * derives the displayed short name (`"main"`) from it.
	 */
	defaultBranchRemoteRef: string | null;
	/**
	 * Number of commits the default branch's remote-tracking ref
	 * has that the current branch's HEAD doesn't — what
	 * `git merge <defaultBranchRemoteRef>` would land. `0` when
	 * the current branch is already up to date with the default,
	 * when we're already on the default branch (the regular
	 * sync button covers that case), or when no default could
	 * be resolved. The SCM panel shows the "Update from main"
	 * affordance iff this is `> 0`.
	 */
	defaultBranchBehind: number;
};

/**
 * Outcome of `git_commit`. Echoed back to the SCM panel so the
 * post-commit toast can show the short SHA and confirm the
 * subject line. Mirrors `moon_protocol::git::GitCommitResult`.
 */
export type GitCommitResult = {
	shortSha: string;
	summary: string;
};

/**
 * One linked working tree of a repository, from
 * `git worktree list --porcelain`. Backs worktree-backed coder
 * sessions (ADR 0028): each isolated session checks its branch out
 * into its own worktree, which the IDE binds as a folder. Mirrors
 * `moon_protocol::git::GitWorktree`.
 */
export type GitWorktree = {
	path: string;
	branch: string | null;
	head: string;
	isMain: boolean;
	isLocked: boolean;
};

/**
 * GitHub permalink (plain URL + Markdown form) for a path + line
 * range, pinned to the current HEAD commit SHA. Mirrors
 * `moon_protocol::git::GitPermalink`.
 */
export type GitPermalink = {
	url: string;
	markdown: string;
};

/**
 * One row in the branch-switcher palette. Discriminated union over
 * `kind`: `local` runs `git switch <name>`, `pr` runs
 * `gh pr checkout <number>` so cross-fork PRs work without manual
 * remote / fetch fiddling. Mirrors `moon_protocol::git::BranchListEntry`.
 */
export type BranchListEntry =
	| {
			kind: 'local';
			name: string;
			lastCommitSubject: string;
			committerDateRelative: string;
			isCurrent: boolean;
			isDefault: boolean;
	  }
	| {
			kind: 'pr';
			number: number;
			title: string;
			author: string;
			headRef: string;
			isDraft: boolean;
			updatedAtRelative: string;
	  };

/**
 * Why the PR section of `BranchList.prs` is empty. The frontend
 * uses this to render the right empty-state row (or suppress the
 * section entirely for `notGithub`). Mirrors
 * `moon_protocol::git::PrListStatus`.
 */
export type PrListStatus =
	| { kind: 'ok' }
	| { kind: 'gh_missing' }
	| { kind: 'gh_not_authed' }
	| { kind: 'not_github' }
	| { kind: 'failed'; detail: string };

/**
 * Result of `branch_list`. Local rows always populate; the PR
 * section's emptiness is annotated by `prStatus`. Mirrors
 * `moon_protocol::git::BranchList`.
 */
export type BranchList = {
	local: BranchListEntry[];
	prs: BranchListEntry[];
	prStatus: PrListStatus;
};

/**
 * Scope filter for `branchList`'s PR section. `all` mirrors `gh
 * pr list --state open` (every open PR in the repo);
 * `participating` runs two `gh pr list --search` queries in
 * parallel — `involves:@me` and `review-requested:@me` — and
 * merges them, mirroring GitHub's notification "Participating"
 * filter plus review-requested. Persisted per folder in
 * `FolderSession.pr_scope`. Mirrors
 * `moon_protocol::git::PrListScope`.
 */
export type PrListScope = 'all' | 'participating';

/**
 * Argument for `branch_switch`. Mirrors
 * `moon_protocol::git::BranchSwitchTarget`.
 */
export type BranchSwitchTarget = { kind: 'local'; name: string } | { kind: 'pr'; number: number };

/**
 * Which baseline the SCM machinery (status entries, change gutter,
 * diff view) compares the working tree against. `head` is the
 * regular `git status` against `HEAD`; `default` substitutes the
 * merge-base with the repo's default branch (`origin/main` /
 * `origin/master`), so the file tree, gutter, and diff view all
 * surface "what does this branch / PR change relative to main".
 * Persisted per folder in `FolderSession.compare_baseline`.
 * Mirrors `moon_protocol::git::CompareBaseline`.
 */
export type CompareBaseline = 'head' | 'default';

/**
 * Result of `git_default_branch_diff`. The frontend caches the
 * `mergeBase` SHA so the diff view + change gutter can pull file
 * content at that rev via `gitRefContent`. Mirrors
 * `moon_protocol::git::BranchDiffStatus`.
 */
export type BranchDiffStatus = {
	mergeBase: string;
	defaultBranchRef: string;
	entries: GitStatusEntry[];
};

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
	/** Slot key of the server that produced this report —
	 * `"typescript"`, `"rust"`, `"oxlint"`, … Lets the frontend
	 * key diagnostics by `(path, producer)` so two servers
	 * (the language server + a co-tenant linter) don't clobber
	 * each other when they both publish for the same file. */
	producer: string;
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
 * One edit inside a single document. Mirrors
 * `moon_protocol::lsp::LspTextEdit`. Edits inside one
 * `LspDocumentEdit` never overlap, so a frontend applier can sort
 * by start position and run them right-to-left.
 */
export type LspTextEdit = {
	range: LspRange;
	newText: string;
};

/**
 * All edits the server wants applied to one document. `path` is
 * workspace-relative. Mirrors `moon_protocol::lsp::LspDocumentEdit`.
 */
export type LspDocumentEdit = {
	path: string;
	edits: LspTextEdit[];
};

/**
 * Result of a `textDocument/rename`. The frontend applies edits
 * to open buffers in memory (marking them dirty) and writes
 * closed-file edits to disk through the workspace host, then
 * fires `workspace/didChangeWatchedFiles` so the server can
 * resync. Mirrors `moon_protocol::lsp::LspWorkspaceEdit`.
 */
export type LspWorkspaceEdit = {
	documentEdits: LspDocumentEdit[];
};

/**
 * Result of `textDocument/prepareRename` — `null` from the IPC
 * means "cursor not on a renameable symbol". Mirrors
 * `moon_protocol::lsp::LspPrepareRename`.
 */
export type LspPrepareRename = {
	range: LspRange;
	placeholder: string;
};

/**
 * One quick-fix the lint tooltip can offer for a diagnostic.
 *
 * Pure-`Command` actions (no edit) and actions whose edit
 * survived translation as empty are dropped on the backend, so
 * every entry the frontend sees has a non-empty `edit` it can
 * apply. Mirrors `moon_protocol::lsp::LspCodeAction`.
 *
 * `producer` is stamped by the broker (the slot key — `typescript`
 * / `oxlint` / `rust`) so the UI can label each action with which
 * co-tenant suggested it. `kind` is LSP's `CodeActionKind` string
 * when the server set one (`quickfix`, `refactor.rewrite`,
 * `source.fixAll.oxc`, …); we don't filter on it today but the
 * data is wired so a future "Show all code actions" surface can.
 */
export type LspCodeAction = {
	title: string;
	kind: string | null;
	edit: LspWorkspaceEdit;
	isPreferred: boolean;
	producer: string;
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
	/**
	 * Primary text edit, when the server picked an exact range to
	 * replace (e.g. completing `foo.bar` from inside `foo`). Falls
	 * back to "replace the matched word with `insertText`/`label`"
	 * when null.
	 */
	textEdit: LspTextEdit | null;
	/**
	 * Edits applied alongside the primary insertion — auto-import
	 * lines, mostly. Typically empty in the initial response and
	 * populated by `completionItem/resolve` (see `resolveToken`).
	 */
	additionalTextEdits: LspTextEdit[];
	/**
	 * Opaque blob the frontend ships back to `lsp_completion_resolve`
	 * to lazy-fetch `additionalTextEdits`. Null when the server
	 * doesn't advertise resolve support — what's in
	 * `additionalTextEdits` is then already final.
	 */
	resolveToken: string | null;
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

/**
 * Severity of a diagnostic log entry. Mirrors `moon_protocol::logs::LogLevel`.
 * Maps to `tracing` levels minus `TRACE`.
 */
export type LogLevel = 'debug' | 'info' | 'warn' | 'error';

/**
 * One line of diagnostic log output, surfaced in the bottom-panel
 * logs view. The `source` field is a free-form bucket key
 * (`lsp.typescript`, `format-on-save`, `editor.completion`, …) that
 * the picker groups by. `seq` is a process-wide monotonic counter
 * so the frontend can merge the snapshot from `logs_snapshot` with
 * the live stream from `logs:entry` without duplicating entries.
 * Mirrors `moon_protocol::logs::LogEntry`.
 */
export type LogEntry = {
	source: string;
	level: LogLevel;
	message: string;
	tsMs: number;
	seq: number;
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
	/** Branch-switcher PR-section filter — see `PrListScope`. */
	pr_scope: PrListScope;
	/** SCM compare baseline — see `CompareBaseline`. */
	compare_baseline: CompareBaseline;
	/** Local-first review-comment drafts for this folder (Phase 5.7). */
	review_comments: ReviewComment[];
	/** Per-file "Viewed" marks for this folder (Phase 5.7). */
	reviewed_files: ReviewedFile[];
	/**
	 * How this folder was bound — persisted so a worktree-backed
	 * coder session's checkout re-binds as a nested worktree folder
	 * on next launch (ADR 0028). See `FolderOrigin`.
	 */
	origin: FolderOrigin;
};

/**
 * Persisted UI session for one workspace. Frontend-owned shape; the
 * backend is pure storage. Mirrors
 * `moon_protocol::session::WorkspaceSession`. Holds one
 * [`FolderSession`] per bound folder, plus a pointer to which folder
 * was active at last save. Lives at `<workspaces_dir>/<id>/session.json`
 * from Phase 7.5 onward — previously it was `AppState.last_session`.
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
 * Coder-specific slice of [`AppState`]. Only frontend-side
 * affordance pointers — actual session content lives under
 * `<XDG_DATA_HOME>/moon-ide/coder-sessions/<project-slug>/<id>.jsonl`.
 * Mirrors `moon_protocol::app_state::CoderAppState`.
 */
export type CoderAppState = {
	/**
	 * Last-opened session id per workspace folder. Restored when
	 * the user revisits a folder; the active folder's entry
	 * decides which session the panel mounts at launch. Mirrors
	 * `moon_protocol::app_state::CoderAppState::last_session_by_folder`.
	 */
	last_session_by_folder: Record<string, string>;
};

/**
 * One user-added OpenAI-compatible provider. Mirrors
 * `moon_protocol::coder_models::CoderProviderConfig`. API keys
 * live in the OS keyring (account=`coder-provider:<id>`), not
 * here — only the `has_api_key` flag is surfaced.
 */
/**
 * Built-in provider flavour. Mirrors
 * `moon_protocol::coder_models::ProviderKind`. The wire path for
 * `custom` and `open_router` is identical (OpenAI-compat
 * `/chat/completions`); `anthropic` triggers the native
 * `/v1/messages` translator on the backend with different auth
 * headers, content blocks, and SSE event grammar.
 */
export type ProviderKind = 'custom' | 'open_router' | 'anthropic';

export type CoderProviderConfig = {
	id: string;
	label: string;
	/**
	 * Built-in flavour, or `custom` for free-form entries.
	 * Defaults to `custom` on the backend so entries persisted
	 * before this field existed deserialize cleanly.
	 */
	kind: ProviderKind;
	base_url: string;
	standard_model: string;
	cheap_model: string;
	has_api_key: boolean;
};

/** Local llama.cpp autocomplete. Mirrors `moon_protocol::next_edit::NextEditAppState`. */
export type NextEditAppState = {
	/** When non-empty, probes/completion use this URL; otherwise `http://{server_host}:{server_port}`. */
	external_base_url: string;
	/** Empty string means resolve `llama-server` from `PATH`. */
	llama_binary: string;
	/** Hugging Face repo id for `llama-server --hf-repo` (e.g. `sweepai/sweep-next-edit-1.5B`). */
	hf_repo: string;
	server_host: string;
	server_port: number;
	/** Managed server only: relaunch starts `llama-server` automatically. */
	server_autostart: boolean;
};

export type NextEditServerStartParams = {
	llamaBinary: string;
	hfRepo: string;
	serverHost: string;
	serverPort: number;
};

export type NextEditServerSnapshot = {
	running: boolean;
	pid: number | null;
	lastExitCode: number | null;
	startError: string | null;
	logTail: string[];
};

export type NextEditProbeKind = 'ready' | 'unreachable' | 'model_loading' | 'error';

export type NextEditProbeResult = {
	kind: NextEditProbeKind;
	detail: string | null;
};

export type NextEditCompleteParams = {
	baseUrl: string;
	relativePath: string;
	cursorLine: number;
	documentText: string;
	headText: string | null;
};

export type NextEditCompleteResult = {
	replacement: string;
	from_line: number;
	to_line: number;
};

/**
 * Per-machine, per-user app state. There is intentionally no `Settings`
 * type — project-level code style lives in `.editorconfig` (Phase 1.5);
 * everything moon-ide stores about a user goes here.
 */
export type AppState = {
	/** Phase 7.2 catalog: every workspace the user has on this
	 * machine. Empty until the user names their first one in
	 * preboot mode. The frontend doesn't mutate this through
	 * `app_state_save` — dedicated IPC owns it
	 * (`workspace_create` / `workspace_delete` / `workspace_rename`). */
	workspaces: WorkspaceMeta[];
	theme: ThemeMode;
	slack: SlackAppState;
	bottom_panel: BottomPanelAppState;
	right_panel: RightPanelKind | null;
	coder: CoderAppState;
	next_edit: NextEditAppState;
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
 *   before dispatching the open call. Process-per-workspace
 *   makes the workspace id implicit on the backend (it's the
 *   process's own).
 */
export type TerminalTarget = { kind: 'host'; cwd: string | null } | { kind: 'container'; cwd: string };

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

/** Default llama-server listen port (IANA dynamic range; avoids 8080 and similar). */
export const DEFAULT_NEXT_EDIT_SERVER_PORT = 53281;
export const DEFAULT_NEXT_EDIT_BASE_URL = `http://127.0.0.1:${DEFAULT_NEXT_EDIT_SERVER_PORT}`;
/** Default Hugging Face repo for managed `llama-server --hf-repo`. */
export const DEFAULT_NEXT_EDIT_HF_REPO = 'sweepai/sweep-next-edit-1.5B';

export const defaultAppState: AppState = {
	workspaces: [],
	theme: 'system',
	slack: { active_bot: null, active_thread_ts: null },
	bottom_panel: { visible: false, height: 240 },
	right_panel: null,
	coder: { last_session_by_folder: {} },
	next_edit: {
		external_base_url: '',
		llama_binary: '',
		hf_repo: DEFAULT_NEXT_EDIT_HF_REPO,
		server_host: '127.0.0.1',
		server_port: DEFAULT_NEXT_EDIT_SERVER_PORT,
		server_autostart: false,
	},
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
	/**
	 * Container is up but attached to no network — endpoint config
	 * wiped by a failed start (typically a host-port conflict).
	 * Unreachable by service name, publishes nothing, regardless of
	 * health. The backend force-recreates it on the next lifecycle
	 * action; until then the row renders as failed.
	 */
	networkless: boolean;
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
	if (svc.networkless) {
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
 * Payload of the `container:state` Tauri event. Process-per-
 * workspace: each window is its own process, so the event
 * implicitly scopes to the current process's workspace. No
 * `workspace_id` field. Mirrors
 * `moon_protocol::container::ContainerStateChange`.
 */
export type ContainerStateChange = {
	status: ContainerStatus;
};

/**
 * Payload of the `editor:request` Tauri event. Emitted by the
 * focus-socket listener (`src-tauri/src/focus_socket.rs`) when
 * the in-container `moon-edit` shim sends an `E\n<path>\n`
 * request — typically the result of `git commit --amend` (or
 * any other tool that respects `$GIT_EDITOR`) running in a
 * container terminal moon-ide opened. The frontend opens the
 * file via `Workspace.openHostFile`, tags it with
 * `pendingEdit = id`, and resolves the parked listener via
 * `ipc.editorForward.finish` / `.cancel` when the user is done.
 * See ADR 0021 and `specs/containers.md` § "Editor forwarding".
 */
export type EditRequest = {
	id: string;
	host_path: string;
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
	folder_path: string;
	project: ProjectComposeStatus;
};

/**
 * Hugging Face user identity returned by `coder_status` and the
 * device-flow completion. Mirrors `moon_coder::auth::HfIdentity`.
 *
 * `orgs` populates the model picker's "Bill to" dropdown. Every
 * entry stays selectable — `can_pay` is a hint, not a gate, because
 * users who declined the optional `orgs` OAuth scope at consent
 * time get back orgs with no `can_pay` / `role_in_org` at all
 * (those fields default to `false` / `null` in that case, which
 * doesn't actually mean "can't bill", just "we don't know"). The
 * router is the source of truth — a rejected bill surfaces
 * verbatim, and the user picks something else. An empty array
 * (or missing field on older payloads) just means "personal
 * account only".
 */
export type HfIdentity = {
	username: string;
	name: string | null;
	avatar_url: string | null;
	email: string | null;
	orgs: HfOrg[];
};

/**
 * One entry of {@link HfIdentity.orgs}. Mirrors
 * `moon_coder::auth::HfOrg`; field names match the Rust struct,
 * not the camelCase shape HF returns on the wire (serde renames at
 * the seam).
 */
export type HfOrg = {
	/**
	 * Display string ("Hugging Face"). Shown in the picker row;
	 * **not** what `X-HF-Bill-To` accepts — use {@link slug} for
	 * the wire value.
	 */
	name: string;
	/**
	 * URL slug ("huggingface"). Sent as `X-HF-Bill-To`. Always
	 * present in current HF userinfo responses since
	 * `preferred_username` ships under the basic `openid profile`
	 * scope; the `null` fallback is defensive paranoia, not a
	 * real path.
	 */
	slug: string | null;
	avatar_url: string | null;
	/**
	 * Authoritative: `true` iff the user can bill inference calls
	 * to this org. Picker disables the matching `<option>` when
	 * this is `false`. Requires the `read-billing` OAuth scope to
	 * be populated; orgs the user didn't authorize at all are
	 * filtered out by {@link role_in_org} before this matters.
	 */
	can_pay: boolean;
	/**
	 * Role string (`"admin"`, `"contributor"`, …) when the user
	 * authorized moon-ide for this specific org at the OAuth
	 * consent screen. `null` means the user is a member but
	 * didn't tick its checkbox — those entries carry no usable
	 * signal and the picker filters them out of the bill-to
	 * dropdown entirely.
	 */
	role_in_org: string | null;
	is_enterprise: boolean;
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

/**
 * One image the user pasted into the composer, shipped over IPC to
 * `coder_send` (and persisted into the session JSONL on the way).
 * Mirrors `moon_coder::ImageAttachment`. `data_url` is the canonical
 * `data:<mime>;base64,<payload>` form — what providers want on the
 * wire and what we hand straight back to the model on session
 * replay. `mime` is duplicated so neither side has to re-parse the
 * data URL prefix when emitting wire messages.
 */
export type ImageAttachmentPayload = {
	data_url: string;
	mime: string;
};

/** Return shape of `coder_unqueue_steer`. `null` from the IPC
 *  means the matching pending steer was already drained — no
 *  un-queue happened. A non-null value carries the original draft
 *  text and pasted images so the panel can repopulate the
 *  composer chip-by-chip. */
export type UnqueuedSteer = {
	text: string;
	images?: ImageAttachmentPayload[];
};

/** Return shape of `coder_revert_to_message`. Carries the dropped
 *  user prompt (text + pasted images) so an "edit & resend" can
 *  prefill the composer; a plain "revert to here" ignores it. The
 *  trimmed transcript itself arrives as `coder:event` replay, not
 *  in this payload. Mirrors `moon_coder::RevertedMessage`. */
export type RevertedMessage = {
	text: string;
	images?: ImageAttachmentPayload[];
};

/** Return shape of `coder_rerun_tool_call`. Carries the tool that
 *  was reapplied plus its fresh dispatch result, so the panel can
 *  confirm the reapply. Mirrors `moon_coder::RerunToolOutcome`. */
export type RerunToolOutcome = {
	tool_name: string;
	result: unknown;
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
	/**
	 * True when the active folder's visible session has the per-session
	 * force-host override engaged. Distinct from `bash_target === 'host'`:
	 * a session resolves to host whenever the container is down (auto),
	 * which is not an override. Drives the "off-default" badge on the
	 * target pip and pre-selects the radio in the override popover.
	 */
	force_host_override: boolean;
};

/**
 * Tagged-union of agent-loop events emitted on the `coder:event`
 * Tauri channel. Mirrors `moon_coder::CoderEvent`. The frontend
 * builds its message list from the running stream — no REST replay,
 * because 6.0 doesn't persist the session.
 */
export type CoderEvent =
	| {
			kind: 'user_message';
			id: string;
			text: string;
			images?: ImageAttachmentPayload[];
			/** `true` when this message is a steer the user sent
			 *  while a turn was already running; it now sits in the
			 *  runner's pending-steers queue and hasn't been drained
			 *  into `messages` yet. The matching `steer_drained`
			 *  event arrives the moment the runner moves it into
			 *  the chat. Defaults to `false` (every non-steer
			 *  message). */
			queued?: boolean;
			/** Unix-ms creation time. Stamped `now` on a live turn,
			 *  carried from the persisted record on replay, so a
			 *  reopened session shows real per-message times. Absent
			 *  for pre-timestamp sessions. */
			created_at_ms?: number | null;
	  }
	| { kind: 'steer_drained'; id: string }
	| { kind: 'assistant_message_start'; id: string }
	| { kind: 'assistant_message_delta'; id: string; delta: string }
	| { kind: 'assistant_thinking_delta'; id: string; delta: string }
	| {
			kind: 'assistant_message_end';
			id: string;
			text: string;
			thinking?: string | null;
			/** Unix-ms creation time, same contract as
			 *  `user_message.created_at_ms`. */
			created_at_ms?: number | null;
	  }
	| { kind: 'tool_call'; id: string; name: string; args: unknown }
	| { kind: 'tool_result'; id: string; result: unknown; is_error: boolean }
	| { kind: 'turn_complete' }
	| { kind: 'aborted' }
	| { kind: 'error'; message: string }
	| { kind: 'session_loaded'; id: string; title: string; created_at_ms: number; updated_at_ms: number }
	| { kind: 'replay'; events: CoderEvent[]; in_flight: boolean }
	| { kind: 'session_title_updated'; id: string; title: string }
	| { kind: 'session_list_changed' }
	| { kind: 'folder_summary_ready'; folder: string; description: string }
	| { kind: 'subagent_spawned'; tool_call_id: string; subagent_id: string; target_folder: string; mode: SubagentMode }
	| { kind: 'subagent_event'; subagent_id: string; inner: CoderEvent }
	| { kind: 'subagent_finished'; subagent_id: string; tokens_used_estimate: number; was_error: boolean }
	| {
			kind: 'token_usage';
			prompt_tokens: number;
			completion_tokens: number;
			total_tokens: number;
			context_window: number;
			source: TokenUsageSource;
			/**
			 * Anthropic prompt-caching breakdown of `prompt_tokens`,
			 * surfaced by OpenRouter when the request used
			 * `cache_control: ephemeral` markers. `cache_read_tokens`
			 * is the slice billed at the 90 %-off cache-read rate;
			 * `cache_creation_tokens` is the slice billed at the
			 * 25 %-surcharge cache-write rate (pays back on the
			 * next call within the 5-min TTL). Both `0` for
			 * non-Anthropic providers and for Anthropic requests
			 * with no cache breakpoints.
			 */
			cache_read_tokens: number;
			cache_creation_tokens: number;
	  }
	| { kind: 'compaction_started'; messages_compacted: number }
	| { kind: 'compaction_complete'; summary: string; prompt_tokens_after: number }
	| { kind: 'hub_sync_started'; session_id: string }
	| { kind: 'hub_sync_finished'; session_id: string; ok: boolean; error?: string };

/**
 * Where the numbers in a `token_usage` event came from. `provider`
 * means the OpenAI-compatible streaming `usage` chunk gave us
 * exact figures; `estimate` means we fell back to a `bytes / 4`
 * approximation because the provider didn't emit one. The UI
 * tints the ring identically and adds a `≈` marker on the
 * tooltip for `estimate`.
 */
export type TokenUsageSource = 'provider' | 'estimate';

/**
 * Two operational modes a sub-agent can run under. Mirrors
 * `moon_coder::tools::CoderMode::as_wire()`. `research` is read-only
 * intent (`write_file`/`edit_file` refuse at the tool boundary; the
 * "no mutation via bash" half is behavioural via the system prompt).
 * `agent` is the full toolkit — same capabilities as the parent.
 * Top-level parent sessions are always `agent` — there is no parent-
 * side toggle.
 */
export type SubagentMode = 'research' | 'agent';

/**
 * One option the agent offered for an `ask_user` question. The
 * user clicks one (or several, for a multi-select question) of
 * these, or types a custom answer instead. Mirrors the
 * `options[]` entries the agent supplies in the `ask_user` tool
 * args.
 */
export type AskUserOption = { id: string; label: string };

/**
 * One question in an `ask_user` prompt. Parsed out of the
 * `tool_call` event's `args.questions[]` by `ToolBodyAskUser`.
 * `allow_multiple` switches the question between single-select
 * (click submits) and multi-select (checkboxes + confirm). The
 * user can always also type a custom free-form answer.
 */
export type AskUserQuestion = {
	id: string;
	question: string;
	options: AskUserOption[];
	allow_multiple?: boolean;
};

/**
 * The user's answer to one `ask_user` question, sent back via
 * `coder_respond_to_prompt`. `selected` is the option ids they
 * clicked; `free_text` is a custom answer they typed. Both can be
 * present (tick a preset and add context); at least one is non-
 * empty for an answered question. Mirrors
 * `moon_coder::QuestionAnswer`.
 */
export type QuestionAnswer = {
	question_id: string;
	selected: string[];
	free_text: string;
};

/**
 * Structured `ask_user` response — one `QuestionAnswer` per
 * question the user actually answered. Mirrors
 * `moon_coder::PromptResponse`. Sent to the backend, which fires
 * the parked oneshot and lets the tool return.
 */
export type PromptResponse = { answers: QuestionAnswer[] };

/**
 * Outer envelope carrying a `(folder, session_id)` tag alongside
 * the inner event. Mirrors `moon_coder::CoderEventEnvelope`. The
 * frontend's multi-session dispatcher routes events to per-
 * `(folder, session_id)` UI buckets — multiple sessions can run
 * concurrently in the same folder (see ADR 0016) so the folder
 * alone isn't enough to disambiguate. Sub-agent events arrive
 * tagged with the **parent's** folder + session id, since sub-
 * agents belong to whichever session originated them.
 *
 * A handful of event variants are genuinely folder-scoped, not
 * session-scoped (`folder_summary_ready`, `hub_sync_started`,
 * `hub_sync_finished`); those arrive with `session_id === ''` and
 * the dispatcher routes them to the folder-level handler rather
 * than a specific session bucket.
 */
export type CoderEventEnvelope = {
	folder: string;
	session_id: string;
	event: CoderEvent;
};

/**
 * Lightweight summary of a persisted coder session — what the
 * panel needs to render the sessions list and the sticky session
 * header. Mirrors `moon_coder::sessions::SessionSummary`.
 */
export type CoderSessionSummary = {
	id: string;
	title: string;
	created_at_ms: number;
	updated_at_ms: number;
	/**
	 * Branch of the git worktree this session runs in, for an
	 * isolated (worktree-backed) session (ADR 0028). Absent for an
	 * ordinary session. Lets the sessions list badge the row.
	 */
	worktreeBranch?: string | null;
	/**
	 * Branch this session's work was committed onto (ADR 0028). Set
	 * when the user commits with the session open; drives the session
	 * list's one-click "switch back to this branch" chip. Absent until
	 * the session's work is first committed.
	 */
	committedBranch?: string | null;
};

/**
 * Result of `coder_new_worktree_session`: the updated workspace
 * snapshot (so the frontend renders the new nested worktree folder)
 * plus the freshly-minted session to open. Mirrors the backend's
 * `NewWorktreeSession`.
 */
export type NewWorktreeSession = {
	workspace: Workspace;
	session: CoderSessionSummary;
};

/**
 * Read/write payload for the model-picker popover. Mirrors
 * `moon_protocol::coder_models::CoderModelSettings`.
 *
 * `standard_model` / `cheap_model` are the wire model ids the
 * router accepts on `chat/completions`, in their final
 * `model:provider` form (`Qwen/Qwen3.5-397B-A17B:scaleway`) —
 * the picker concatenates on click so the runner never has to.
 * Empty strings mean "use the hardcoded default" — the runner
 * substitutes `DEFAULT_STANDARD_MODEL` / `DEFAULT_CHEAP_MODEL` at
 * request time.
 *
 * `bill_to` is the HF org slug for `X-HF-Bill-To`. Empty = bill
 * the user's personal account. Only applies when `active_provider`
 * is `null` (HF route); when a user provider is active, the
 * runner suppresses the header.
 *
 * `active_provider` is `null` for the implicit HF default or
 * `id` of one of the entries in `providers`.
 */
export type CoderModelSettings = {
	standard_model: string;
	cheap_model: string;
	bill_to: string;
	active_provider: string | null;
	providers: CoderProviderConfig[];
	/** Per-slug context-window cap in tokens. Slug = full wire id
	 *  (with any `:provider` suffix for HF, bare id for user
	 *  providers). Missing entry / value `0` = use the model's
	 *  catalog window directly; the runner clamps with
	 *  `min(catalog, cap)` everywhere `context_window` is read
	 *  (usage ring, auto-compaction). Use case: capping a 1M-window
	 *  model at 250k where quality degrades past that point. */
	context_window_overrides: Record<string, number>;
	/** Per-workspace lock on the active provider. `null` (or
	 *  missing) means "no lock — follow the global default and
	 *  let modal saves write the global". Non-null pins the
	 *  workspace to the chosen provider; the modal's save flow
	 *  then writes the lock into `session.json` and leaves the
	 *  global default untouched, so toggling provider in another
	 *  workspace doesn't drag this one along. */
	provider_lock?: CoderProviderLock | null;
};

/** Per-workspace lock on the coder's active provider. Mirrors
 *  `moon_protocol::coder_models::CoderProviderLock` — a tagged
 *  union so the "locked to HF" state is distinguishable from
 *  "no lock" (both would collapse to `null` in an
 *  `Option<Option<string>>` shape). */
export type CoderProviderLock = { kind: 'hf' } | { kind: 'user'; id: string };

/** Result of `coder_probe_provider`. Mirrors
 *  `moon_protocol::coder_models::ProviderProbeResult`. */
export type ProviderProbeResult = {
	model_count: number;
	sample_model_ids: string[];
};

/**
 * Catalog entry for a user-added provider. Mirrors
 * `moon_protocol::coder_models::ProviderModelSummary`.
 *
 * Everything but `id` is optional and server-dependent — the
 * OpenAI-compat spec only promises `id`/`owned_by`. OpenRouter
 * ships the richer fields (name, context, pricing, description)
 * and the picker lights them up automatically. Pricing is
 * normalised at the backend to **$/million tokens** regardless
 * of wire shape, so the UI never has to know whether the source
 * was OpenRouter's per-token strings or LiteLLM's per-million
 * floats.
 */
export type ProviderModelSummary = {
	id: string;
	owned_by: string | null;
	name?: string | null;
	context_length?: number | null;
	pricing_in_per_million?: number | null;
	pricing_out_per_million?: number | null;
	description?: string | null;
};

/**
 * One row of the router catalog the picker renders. Mirrors
 * `moon_protocol::coder_models::RouterModel`. The router returns
 * the list sorted "most popular first" and we preserve that order
 * verbatim — the picker only deviates when the user types into the
 * search box.
 */
export type RouterModel = {
	id: string;
	owned_by: string;
	supports_tools_anywhere: boolean;
	providers: RouterProvider[];
};

export type RouterProvider = {
	provider: string;
	context_length: number | null;
	supports_tools: boolean;
	pricing: RouterPricing | null;
	/**
	 * Mean time-to-first-token in milliseconds, from the router's
	 * internal probes. `null` for providers the router doesn't have
	 * measurements for (typically `featherless-ai` entries on a
	 * model's first day, or very low-traffic routes).
	 */
	first_token_latency_ms: number | null;
	/**
	 * Mean output throughput in tokens-per-second from the same
	 * probes. Same `null` semantics as `first_token_latency_ms`.
	 */
	throughput: number | null;
};

export type RouterPricing = {
	input: number;
	output: number;
};

/**
 * One declared host-to-dev port forward. Mirrors
 * `moon_protocol::ports::ForwardedPort`. `host_port` defaults
 * to `container_port` on the picker; the user retypes it on
 * cross-workspace conflicts.
 */
export type ForwardedPort = {
	container_port: number;
	host_port: number;
	label: string;
};

/** Mirrors `moon_protocol::ports::ForwardedPortHealth`. */
export type ForwardedPortHealth = 'live' | 'host_port_busy' | 'proxy_down';

/** Mirrors `moon_protocol::ports::ForwardedPortStatus`. */
export type ForwardedPortStatus = {
	forward: ForwardedPort;
	health: ForwardedPortHealth;
};

/** Mirrors `moon_protocol::ports::PortsApplyResult`. */
export type PortsApplyResult = {
	applied: ForwardedPort[];
	conflicts: ForwardedPort[];
};

/** One HF Hub bucket bound to a workspace. Mirrors
 *  `moon_protocol::coder_hub::CoderHubBucket`. The runner pushes
 *  session JSONLs to `<namespace>/<name>` on the Hub; `autosync`
 *  flips the per-`TurnEnded` push on and off. `uploaded` is the
 *  local "what we last sent" cache so unchanged JSONLs skip the
 *  round-trip. */
export type CoderHubBucket = {
	namespace: string;
	name: string;
	private: boolean;
	autosync: boolean;
	uploaded: Record<string, UploadedMarker>;
};

/** Per-session push bookkeeping. Mirrors
 *  `moon_protocol::coder_hub::UploadedMarker`. */
export type UploadedMarker = {
	bytes: number;
	at_ms: number;
};

/** One option in the connect modal's namespace dropdown.
 *  Mirrors `moon_protocol::coder_hub::HubNamespace`. */
export type HubNamespace = { kind: 'user'; name: string } | { kind: 'org'; name: string };

/** Result of the bulk "Upload all sessions" affordance. Mirrors
 *  `moon_protocol::coder_hub::HubUploadAllSummary`. `uploaded` +
 *  `skipped` + `failed.length` is the total session count touched
 *  by the run — `skipped` is "already at-length on the Hub" and
 *  bypasses the round-trip entirely. */
export type HubUploadAllSummary = {
	uploaded: number;
	skipped: number;
	failed: HubUploadFailure[];
};

/** Per-session failure detail in [`HubUploadAllSummary`]. Mirrors
 *  `moon_protocol::coder_hub::HubUploadFailure`. */
export type HubUploadFailure = {
	session_id: string;
	error: string;
};

/** Live status reported by the runner while a session JSONL is
 *  being pushed to the Hub. Drives the cloud-icon state on the
 *  session-list rows. Per-folder via `CoderEventEnvelope`. */
export type HubSyncProgress =
	| { kind: 'started'; session_id: string }
	| { kind: 'finished'; session_id: string; ok: boolean; error?: string };

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

/**
 * Render any error value as a single human-readable string.
 *
 * - `MoonError` (the JSON shape every Tauri command returns
 *   when it fails) is rendered as just its `message` — the
 *   `code` discriminant is internal taxonomy, not something
 *   the user needs to read. Callers that genuinely care
 *   about `code` should `isMoonError` and switch on it
 *   themselves.
 * - `Error` instances render as their `message`.
 * - Strings pass through unchanged.
 * - Anything else (plain objects with a `message`, etc.)
 *   gets a best-effort coercion before falling back to
 *   `String(err)`. The `[object Object]` failure mode is
 *   the explicit thing this function exists to prevent.
 */
export function formatError(err: unknown): string {
	if (isMoonError(err)) {
		return err.message;
	}
	if (err instanceof Error) {
		return err.message;
	}
	if (typeof err === 'string') {
		return err;
	}
	if (typeof err === 'object' && err !== null) {
		const maybe = err as { message?: unknown };
		if (typeof maybe.message === 'string') {
			return maybe.message;
		}
		try {
			return JSON.stringify(err);
		} catch {
			return 'unknown error';
		}
	}
	return String(err);
}
