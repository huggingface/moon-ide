//! LSP surface exposed to the UI.
//!
//! **Not** raw LSP — these are moon-shaped subsets of the LSP types
//! the frontend actually consumes. The broker in `moon-core` translates
//! `lsp-types` into these before they cross the Tauri boundary. Two
//! reasons for the translation layer:
//!
//! 1. LSP's versioning, request-id machinery, and optional fields
//!    aren't something the UI should be reasoning about.
//! 2. We want a stable, evolvable wire schema. If upstream LSP adds a
//!    field we don't care about, our binding doesn't need to change.
//!    If we rename something, we do it here in one place.
//!
//! **Position encoding**: all positions are UTF-16 code units (LSP's
//! default, and CodeMirror's native string offset). Line and character
//! are zero-based.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
#[serde(rename_all = "lowercase")]
pub enum LspSeverity {
	Error,
	Warning,
	Info,
	Hint,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
pub struct LspPosition {
	pub line: u32,
	pub character: u32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
pub struct LspRange {
	pub start: LspPosition,
	pub end: LspPosition,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
pub struct LspDiagnostic {
	pub range: LspRange,
	pub severity: LspSeverity,
	pub message: String,
	/// Producer of the diagnostic, e.g. `"ts"`, `"typescript"`,
	/// `"eslint"`. Rendered as a tag in the tooltip so a user facing
	/// a "disagreement" between two tools can tell whose opinion is
	/// on screen.
	pub source: Option<String>,
	/// Rule / error code as a string. Some servers emit integers, some
	/// emit strings — we stringify during translation so the frontend
	/// can just `Display` it.
	pub code: Option<String>,
}

/// Pushed on the `lsp:diagnostics` Tauri event whenever a server
/// emits `textDocument/publishDiagnostics`. `path` is the same
/// workspace-relative form the frontend opens files with; the
/// backend translates between that and `file://` URIs internally.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct LspDiagnosticsEvent {
	pub path: String,
	pub diagnostics: Vec<LspDiagnostic>,
}

/// Target of a `textDocument/definition` (or equivalent) jump.
///
/// Two shapes are possible:
///
/// - **In-workspace**: `path` is the same workspace-relative form the
///   frontend opens files with. The UI routes through its normal open-file
///   machinery so the tab strip, focus ring, and editor state all come up
///   exactly like a manual open.
/// - **External**: the definition lives outside the workspace root (e.g.
///   a type in `node_modules/`, a `.d.ts` in the Rust toolchain). We set
///   `external_uri` to the original `file://…` URI and leave `path`
///   empty. The UI currently surfaces a muted toast (`"goto-definition:
///   outside workspace"`) rather than silently opening nothing — full
///   external-file support lands when we grow a read-only viewer.
///
/// Exactly one of `path` / `external_uri` is non-empty. We encode that
/// as two optionally-empty fields rather than an enum because `ts-rs`'
/// externally-tagged enums are clunky to consume from CodeMirror's
/// view callbacks.
#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct LspLocation {
	/// Workspace-relative path, or empty string when the target is
	/// outside the workspace root.
	pub path: String,
	/// Range to select / reveal in the target file. `range.start` is
	/// where we place the caret.
	pub range: LspRange,
	/// Original LSP URI when the target is outside the workspace.
	/// Empty for in-workspace hits.
	pub external_uri: String,
}

/// Response to a hover request. `contents` is pre-rendered Markdown
/// the UI can drop into a markdown-it instance; we normalise
/// `MarkedString` / `MarkupContent` / plaintext on the broker side so
/// the UI has one branch.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct LspHover {
	pub contents: String,
	/// Range covered by the hover, if the server provided one. CM
	/// uses this to decide when to dismiss the tooltip as the caret
	/// moves.
	pub range: Option<LspRange>,
}

/// Subset of LSP's `CompletionItemKind`. We keep the full list rather
/// than folding (e.g.) `Struct` into `Class`: the UI uses the kind
/// only for iconography, and collapsing kinds would lose information
/// that's cheap to carry.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
#[serde(rename_all = "lowercase")]
pub enum LspCompletionKind {
	Text,
	Method,
	Function,
	Constructor,
	Field,
	Variable,
	Class,
	Interface,
	Module,
	Property,
	Unit,
	Value,
	Enum,
	Keyword,
	Snippet,
	Color,
	File,
	Reference,
	Folder,
	EnumMember,
	Constant,
	Struct,
	Event,
	Operator,
	TypeParameter,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct LspCompletionItem {
	pub label: String,
	pub kind: Option<LspCompletionKind>,
	/// Short right-hand annotation: signature summary, type name, etc.
	pub detail: Option<String>,
	/// Longer markdown body shown in the selected-item side panel.
	pub documentation: Option<String>,
	/// Text inserted if different from `label`. CM's autocomplete
	/// uses `label` as the display text and falls back to it as the
	/// insert text when this is `None`.
	pub insert_text: Option<String>,
	/// Sort key if the server wants a specific order — CM sorts by
	/// `label` by default, which is sometimes worse (e.g. private
	/// `_` members bubble to the top).
	pub sort_text: Option<String>,
	/// Filter key if the server wants a different match surface than
	/// `label`. Same rationale as `sort_text`.
	pub filter_text: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct LspCompletionList {
	/// `true` when the server truncated — CM should re-query on the
	/// next keystroke rather than trust the current list as final.
	pub is_incomplete: bool,
	pub items: Vec<LspCompletionItem>,
}

/// Per-language server availability reported on the
/// `lsp:status` Tauri event. We emit on every transition so the
/// status bar can paint without polling.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
#[serde(rename_all = "lowercase")]
pub enum LspServerStatus {
	/// Binary isn't on PATH (or the workspace host refused to spawn
	/// it). UI surfaces a quiet pill suggesting the user install it.
	NotAvailable,
	Starting,
	Running,
	/// Crashed once. The broker will auto-restart on the next open,
	/// but we surface the state so a crash-loop is visible.
	Crashed,
	Stopped,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct LspStatusEvent {
	/// Language id per LSP convention: `typescript`, `typescriptreact`,
	/// `javascript`, `javascriptreact`, later `rust`, `svelte`, `css`,
	/// etc. Stable across the protocol.
	pub language_id: String,
	pub status: LspServerStatus,
	/// Short human-readable message for the status pill tooltip when
	/// we have something to say (binary name, crash reason, etc.).
	pub detail: Option<String>,
}
