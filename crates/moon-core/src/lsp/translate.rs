//! Translate between `lsp-types` (upstream LSP shapes) and
//! `moon_protocol::lsp` (what the UI consumes).
//!
//! Single place that knows the upstream crate exists. Keeps the rest
//! of `moon-core::lsp` dealing in either our types or raw JSON, never
//! both. When `lsp-types` changes an enum shape or adds a variant we
//! care about, this file is where the fix lands.

use lsp_types as lt;
use moon_protocol::lsp as mp;
use std::path::Path;

pub fn diagnostic(d: lt::Diagnostic) -> mp::LspDiagnostic {
	mp::LspDiagnostic {
		range: range(d.range),
		severity: severity(d.severity),
		message: d.message,
		source: d.source,
		code: d.code.map(|c| match c {
			lt::NumberOrString::Number(n) => n.to_string(),
			lt::NumberOrString::String(s) => s,
		}),
	}
}

pub fn range(r: lt::Range) -> mp::LspRange {
	mp::LspRange {
		start: position(r.start),
		end: position(r.end),
	}
}

pub fn position(p: lt::Position) -> mp::LspPosition {
	mp::LspPosition {
		line: p.line,
		character: p.character,
	}
}

pub fn to_lsp_position(p: mp::LspPosition) -> lt::Position {
	lt::Position {
		line: p.line,
		character: p.character,
	}
}

pub fn to_lsp_range(r: &mp::LspRange) -> lt::Range {
	lt::Range {
		start: to_lsp_position(r.start),
		end: to_lsp_position(r.end),
	}
}

/// Reconstruct an `lsp_types::Diagnostic` from the protocol shape
/// the frontend round-trips (range + severity + message + source +
/// code). Used as the `context.diagnostics` payload of a
/// `textDocument/codeAction` request — servers (oxlint in
/// particular) match incoming diagnostics back to their internal
/// representation by `(range, code)`, so getting those two fields
/// right is the only thing that matters for the request to find
/// the right fixes. `related_information`, `tags`, and `data`
/// aren't round-tripped through our protocol so they're left
/// `None`; servers that need them for code-action matching are
/// not represented in our wired set today.
pub fn to_lsp_diagnostic(d: &mp::LspDiagnostic) -> lt::Diagnostic {
	lt::Diagnostic {
		range: to_lsp_range(&d.range),
		severity: Some(match d.severity {
			mp::LspSeverity::Error => lt::DiagnosticSeverity::ERROR,
			mp::LspSeverity::Warning => lt::DiagnosticSeverity::WARNING,
			mp::LspSeverity::Info => lt::DiagnosticSeverity::INFORMATION,
			mp::LspSeverity::Hint => lt::DiagnosticSeverity::HINT,
		}),
		code: d.code.as_ref().map(|c| lt::NumberOrString::String(c.clone())),
		code_description: None,
		source: d.source.clone(),
		message: d.message.clone(),
		related_information: None,
		tags: None,
		data: None,
	}
}

fn severity(s: Option<lt::DiagnosticSeverity>) -> mp::LspSeverity {
	// Default to Error when the server doesn't specify: playing
	// safe is louder than silent, and most servers always set this
	// anyway (tsserver, rust-analyzer, etc.).
	match s {
		Some(lt::DiagnosticSeverity::ERROR) | None => mp::LspSeverity::Error,
		Some(lt::DiagnosticSeverity::WARNING) => mp::LspSeverity::Warning,
		Some(lt::DiagnosticSeverity::INFORMATION) => mp::LspSeverity::Info,
		Some(lt::DiagnosticSeverity::HINT) => mp::LspSeverity::Hint,
		// LSP reserves 5..= for proposals; map anything we don't
		// recognise to Info so it stays visible but not alarming.
		Some(_) => mp::LspSeverity::Info,
	}
}

/// Normalise every LSP "hover contents" flavour into a single Markdown
/// string. The frontend runs it through markdown-it either way; we
/// strip obvious noise (empty strings, all-whitespace fragments) so
/// an empty `{}` response doesn't open a blank tooltip.
pub fn hover(h: lt::Hover) -> Option<mp::LspHover> {
	let body = hover_contents(h.contents);
	let trimmed = body.trim();
	if trimmed.is_empty() {
		return None;
	}
	Some(mp::LspHover {
		contents: trimmed.to_owned(),
		range: h.range.map(range),
	})
}

fn hover_contents(c: lt::HoverContents) -> String {
	match c {
		lt::HoverContents::Scalar(s) => marked_string(s),
		lt::HoverContents::Array(items) => items
			.into_iter()
			.map(marked_string)
			.collect::<Vec<_>>()
			.join("\n\n---\n\n"),
		lt::HoverContents::Markup(m) => {
			// PlainText → wrap in a fenced block so markdown-it
			// doesn't try to interpret `>` / `*` characters.
			match m.kind {
				lt::MarkupKind::Markdown => m.value,
				lt::MarkupKind::PlainText => format!("```\n{}\n```", m.value),
			}
		}
	}
}

fn marked_string(s: lt::MarkedString) -> String {
	match s {
		lt::MarkedString::String(text) => text,
		lt::MarkedString::LanguageString(ls) => {
			// LanguageString means "this is a code sample in
			// <language>", so wrap it in a fenced block that the
			// UI's Shiki/markdown-it will highlight.
			format!("```{}\n{}\n```", ls.language, ls.value)
		}
	}
}

pub fn completion_kind(k: lt::CompletionItemKind) -> mp::LspCompletionKind {
	use mp::LspCompletionKind as M;
	match k {
		lt::CompletionItemKind::TEXT => M::Text,
		lt::CompletionItemKind::METHOD => M::Method,
		lt::CompletionItemKind::FUNCTION => M::Function,
		lt::CompletionItemKind::CONSTRUCTOR => M::Constructor,
		lt::CompletionItemKind::FIELD => M::Field,
		lt::CompletionItemKind::VARIABLE => M::Variable,
		lt::CompletionItemKind::CLASS => M::Class,
		lt::CompletionItemKind::INTERFACE => M::Interface,
		lt::CompletionItemKind::MODULE => M::Module,
		lt::CompletionItemKind::PROPERTY => M::Property,
		lt::CompletionItemKind::UNIT => M::Unit,
		lt::CompletionItemKind::VALUE => M::Value,
		lt::CompletionItemKind::ENUM => M::Enum,
		lt::CompletionItemKind::KEYWORD => M::Keyword,
		lt::CompletionItemKind::SNIPPET => M::Snippet,
		lt::CompletionItemKind::COLOR => M::Color,
		lt::CompletionItemKind::FILE => M::File,
		lt::CompletionItemKind::REFERENCE => M::Reference,
		lt::CompletionItemKind::FOLDER => M::Folder,
		lt::CompletionItemKind::ENUM_MEMBER => M::EnumMember,
		lt::CompletionItemKind::CONSTANT => M::Constant,
		lt::CompletionItemKind::STRUCT => M::Struct,
		lt::CompletionItemKind::EVENT => M::Event,
		lt::CompletionItemKind::OPERATOR => M::Operator,
		lt::CompletionItemKind::TYPE_PARAMETER => M::TypeParameter,
		// Upstream reserves extensions we don't recognise; mapping
		// them to Text is the safest display fallback.
		_ => M::Text,
	}
}

pub fn completion_item(item: lt::CompletionItem) -> mp::LspCompletionItem {
	completion_item_with_resolve(item, true)
}

/// Same projection as [`completion_item`] but lets the caller
/// suppress the resolve token. Used by
/// [`crate::lsp::server::LspServer::completion_resolve`]: the
/// resolved item is final, so handing the frontend a token that
/// would re-trigger another `completionItem/resolve` round-trip
/// would just spin.
pub fn completion_item_with_resolve(item: lt::CompletionItem, include_resolve_token: bool) -> mp::LspCompletionItem {
	let documentation = item.documentation.clone().map(|d| match d {
		lt::Documentation::String(s) => s,
		lt::Documentation::MarkupContent(m) => match m.kind {
			lt::MarkupKind::Markdown => m.value,
			lt::MarkupKind::PlainText => format!("```\n{}\n```", m.value),
		},
	});
	let text_edit = item.text_edit.as_ref().and_then(completion_primary_edit);
	let additional_text_edits: Vec<mp::LspTextEdit> = item
		.additional_text_edits
		.clone()
		.unwrap_or_default()
		.into_iter()
		.map(text_edit_owned)
		.collect();
	// Round-trip the entire `lt::CompletionItem` as JSON so the
	// resolve call can hand it back to the server verbatim. LSP
	// servers are picky about getting back exactly the item they
	// sent — projecting through our shape and reconstructing
	// would lose `data`, server-internal fields, etc., and
	// `tsserver` flatly errors when its `data` blob is missing.
	let resolve_token = if include_resolve_token {
		serde_json::to_string(&item).ok()
	} else {
		None
	};
	mp::LspCompletionItem {
		label: item.label,
		kind: item.kind.map(completion_kind),
		detail: item.detail,
		documentation,
		insert_text: item.insert_text,
		sort_text: item.sort_text,
		filter_text: item.filter_text,
		text_edit,
		additional_text_edits,
		resolve_token,
	}
}

/// Project the `text_edit` field of an `lt::CompletionItem` down
/// to one of our `LspTextEdit`s. LSP gives the server a choice
/// between a plain `TextEdit` and an `InsertReplaceEdit` (where
/// the server wants different ranges for "insert mode" — type
/// continues — and "replace mode" — the matched word gets
/// rewritten). We declared `insert_replace_support: false` in
/// our client capabilities, so well-behaved servers send a plain
/// `TextEdit`; we still cope with the replace shape and pick the
/// **replace** range — that's what the user means when they
/// commit a completion that "rewrites" the in-flight token.
fn completion_primary_edit(edit: &lt::CompletionTextEdit) -> Option<mp::LspTextEdit> {
	match edit {
		lt::CompletionTextEdit::Edit(te) => Some(mp::LspTextEdit {
			range: range(te.range),
			new_text: te.new_text.clone(),
		}),
		lt::CompletionTextEdit::InsertAndReplace(ir) => Some(mp::LspTextEdit {
			range: range(ir.replace),
			new_text: ir.new_text.clone(),
		}),
	}
}

fn text_edit_owned(e: lt::TextEdit) -> mp::LspTextEdit {
	mp::LspTextEdit {
		range: range(e.range),
		new_text: e.new_text,
	}
}

/// Project a `textDocument/definition` (or `typeDefinition`,
/// `implementation`) response down to a single `LspLocation`.
///
/// LSP lets servers return an array or a `LocationLink` list. When
/// more than one target is offered (rare for definition, common for
/// implementations), we take the first — the UI is "jump" not "pick
/// one of several", and a disambiguation dropdown is a later-stage
/// UX feature.
///
/// Returns `None` for an empty response or when a target can't be
/// translated to either a workspace-relative path or a `file://` URI
/// the UI can display.
///
/// `root` is used to make in-workspace targets relative; external
/// targets are surfaced via `external_uri`.
pub fn definition_response(resp: lt::GotoDefinitionResponse, root: &Path) -> Option<mp::LspLocation> {
	match resp {
		lt::GotoDefinitionResponse::Scalar(loc) => location(loc, root),
		lt::GotoDefinitionResponse::Array(locs) => locs.into_iter().find_map(|l| location(l, root)),
		lt::GotoDefinitionResponse::Link(links) => links.into_iter().find_map(|l| location_link(l, root)),
	}
}

fn location(loc: lt::Location, root: &Path) -> Option<mp::LspLocation> {
	let (path, external_uri) = resolve_uri(&loc.uri, root);
	Some(mp::LspLocation {
		path,
		range: range(loc.range),
		external_uri,
	})
}

fn location_link(link: lt::LocationLink, root: &Path) -> Option<mp::LspLocation> {
	let (path, external_uri) = resolve_uri(&link.target_uri, root);
	// Servers provide both `target_range` (full definition span,
	// e.g. the whole function body) and `target_selection_range`
	// (just the identifier). The UI jumps to the identifier — that
	// matches what every other editor does and lands the caret where
	// the user can actually rename / paste / type.
	Some(mp::LspLocation {
		path,
		range: range(link.target_selection_range),
		external_uri,
	})
}

/// Turn an LSP URI into `(workspace_relative_path, external_uri)`.
/// Exactly one of the two returned strings is non-empty.
fn resolve_uri(uri: &lt::Uri, root: &Path) -> (String, String) {
	// `lsp_types::Uri` is a `fluent_uri` newtype with no
	// `to_file_path` helper. Parse through `url::Url` instead; the
	// LSP string form is identical to the URL crate's parse input.
	let Ok(parsed) = url::Url::parse(uri.as_str()) else {
		return (String::new(), uri.as_str().to_owned());
	};
	let Ok(abs) = parsed.to_file_path() else {
		return (String::new(), uri.as_str().to_owned());
	};
	match abs.strip_prefix(root) {
		Ok(rel) => (rel.to_string_lossy().replace('\\', "/"), String::new()),
		Err(_) => (String::new(), uri.as_str().to_owned()),
	}
}

/// Translate a `textDocument/prepareRename` response. Returns
/// `None` for the "not renameable" / "default behaviour" cases —
/// the UI treats both identically (no rename surface).
///
/// `fallback_word` is the word under the cursor as the frontend
/// would compute it; used as the placeholder when the server
/// returned a bare range with no placeholder (the common shape:
/// servers say "yes, rename this span" without echoing the
/// existing identifier).
pub fn prepare_rename_response(resp: lt::PrepareRenameResponse, fallback_word: &str) -> Option<mp::LspPrepareRename> {
	match resp {
		lt::PrepareRenameResponse::Range(r) => Some(mp::LspPrepareRename {
			range: range(r),
			placeholder: fallback_word.to_owned(),
		}),
		lt::PrepareRenameResponse::RangeWithPlaceholder { range: r, placeholder } => Some(mp::LspPrepareRename {
			range: range(r),
			placeholder,
		}),
		// `DefaultBehavior { default_behavior: true }` means
		// "use the client's own word-at-position logic". We
		// have that on the frontend (CM's `wordAt`) — but
		// surfacing that as "no prepare data" lets the caller
		// fall back to the trigger position without us having
		// to invent a synthetic range here.
		lt::PrepareRenameResponse::DefaultBehavior { default_behavior } if default_behavior => Some(mp::LspPrepareRename {
			range: mp::LspRange {
				start: mp::LspPosition { line: 0, character: 0 },
				end: mp::LspPosition { line: 0, character: 0 },
			},
			placeholder: fallback_word.to_owned(),
		}),
		lt::PrepareRenameResponse::DefaultBehavior { .. } => None,
	}
}

/// Flatten an LSP `WorkspaceEdit` into the protocol shape the
/// frontend applies. Drops any entries whose target URI isn't a
/// `file://` URI under `root` — the UI can't (yet) reach files
/// outside the active folder, and surfacing partial cross-folder
/// edits would silently lose user intent. Cross-folder rename
/// support lands when we grow the multi-bound-folder edit path.
///
/// Both wire shapes are flattened: the legacy `changes` map and
/// the newer `document_changes` array (we ignore `RenameFile` /
/// `CreateFile` / `DeleteFile` resource ops — see the
/// `LspWorkspaceEdit` docs).
pub fn workspace_edit(edit: lt::WorkspaceEdit, root: &Path) -> mp::LspWorkspaceEdit {
	let mut document_edits: Vec<mp::LspDocumentEdit> = Vec::new();
	if let Some(changes) = edit.changes {
		for (uri, edits) in changes {
			let (path, _ext) = resolve_uri(&uri, root);
			if path.is_empty() {
				continue;
			}
			document_edits.push(mp::LspDocumentEdit {
				path,
				edits: edits.into_iter().map(text_edit).collect(),
			});
		}
	}
	if let Some(doc_changes) = edit.document_changes {
		match doc_changes {
			lt::DocumentChanges::Edits(edits) => {
				for doc in edits {
					let (path, _ext) = resolve_uri(&doc.text_document.uri, root);
					if path.is_empty() {
						continue;
					}
					let edits: Vec<mp::LspTextEdit> = doc
						.edits
						.into_iter()
						.map(|e| match e {
							lt::OneOf::Left(te) => text_edit(te),
							lt::OneOf::Right(ann) => text_edit(ann.text_edit),
						})
						.collect();
					document_edits.push(mp::LspDocumentEdit { path, edits });
				}
			}
			lt::DocumentChanges::Operations(_) => {
				// File-creation / -rename / -deletion ops fall
				// outside the safe surface for an LSP rename —
				// see `LspWorkspaceEdit`. Servers asking for
				// them get the identifier edits applied; the
				// resource ops are silently dropped.
			}
		}
	}
	mp::LspWorkspaceEdit { document_edits }
}

fn text_edit(e: lt::TextEdit) -> mp::LspTextEdit {
	mp::LspTextEdit {
		range: range(e.range),
		new_text: e.new_text,
	}
}

/// Project a `textDocument/codeAction` response into the
/// frontend-shaped `LspCodeAction` list. Pure-`Command` actions
/// (no `edit`) are dropped — see [`mp::LspCodeAction`] for why.
/// `producer` is stamped here so the frontend can label each
/// action with which co-tenant suggested it (oxlint vs tsgo)
/// without the server having to carry that string itself.
pub fn code_actions(resp: lt::CodeActionResponse, producer: &str, root: &Path) -> Vec<mp::LspCodeAction> {
	let mut out = Vec::with_capacity(resp.len());
	for entry in resp {
		let action = match entry {
			lt::CodeActionOrCommand::CodeAction(a) => a,
			// `Command` shape: server hands us a workspace command
			// to invoke instead of edits. We don't run commands
			// (no `workspace/executeCommand` plumbing), so silently
			// drop these. The "Disable rule" / autofix actions we
			// actually want from oxlint all come through as
			// `CodeAction` with edits.
			lt::CodeActionOrCommand::Command(_) => continue,
		};
		let Some(edit) = action.edit else {
			// No-edit code actions are typically command-only
			// (`oxc.fixAll`); drop for the same reason as above.
			continue;
		};
		let workspace = workspace_edit(edit, root);
		if workspace.document_edits.is_empty() {
			continue;
		}
		out.push(mp::LspCodeAction {
			title: action.title,
			kind: action.kind.map(|k| k.as_str().to_owned()),
			edit: workspace,
			is_preferred: action.is_preferred.unwrap_or(false),
			producer: producer.to_owned(),
		});
	}
	out
}

pub fn completion_response(resp: lt::CompletionResponse) -> mp::LspCompletionList {
	completion_response_with_resolve(resp, true)
}

/// Same as [`completion_response`] but lets the broker pass the
/// server's `resolveProvider` flag through. We only emit a
/// `resolve_token` when the server actually supports
/// `completionItem/resolve`; otherwise the frontend would chase
/// a round-trip whose response is identical to what we already
/// projected. Saves a render-blocking IPC on every accept for
/// servers without resolve.
pub fn completion_response_with_resolve(resp: lt::CompletionResponse, supports_resolve: bool) -> mp::LspCompletionList {
	match resp {
		lt::CompletionResponse::Array(items) => mp::LspCompletionList {
			is_incomplete: false,
			items: items
				.into_iter()
				.map(|i| completion_item_with_resolve(i, supports_resolve))
				.collect(),
		},
		lt::CompletionResponse::List(list) => mp::LspCompletionList {
			is_incomplete: list.is_incomplete,
			items: list
				.items
				.into_iter()
				.map(|i| completion_item_with_resolve(i, supports_resolve))
				.collect(),
		},
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn diagnostic_severity_defaults_to_error() {
		let d = lt::Diagnostic {
			range: lt::Range::default(),
			severity: None,
			code: None,
			code_description: None,
			source: None,
			message: "boom".into(),
			related_information: None,
			tags: None,
			data: None,
		};
		assert_eq!(diagnostic(d).severity, mp::LspSeverity::Error);
	}

	#[test]
	fn hover_empty_contents_yields_none() {
		let h = lt::Hover {
			contents: lt::HoverContents::Scalar(lt::MarkedString::String("   ".into())),
			range: None,
		};
		assert!(hover(h).is_none());
	}

	/// In-workspace target → `path` populated, `external_uri` blank.
	#[test]
	fn definition_in_workspace_becomes_relative() {
		use std::str::FromStr;
		let tmp = tempfile::tempdir().unwrap();
		let file = tmp.path().join("src").join("lib.rs");
		std::fs::create_dir_all(file.parent().unwrap()).unwrap();
		std::fs::write(&file, b"").unwrap();
		let uri_str = format!("file://{}", file.display());
		let loc = lt::Location {
			uri: lt::Uri::from_str(&uri_str).unwrap(),
			range: lt::Range::default(),
		};
		let resp = definition_response(lt::GotoDefinitionResponse::Scalar(loc), tmp.path()).expect("location");
		assert_eq!(resp.path, "src/lib.rs");
		assert!(resp.external_uri.is_empty());
	}

	/// External target (outside the workspace root) → `external_uri`
	/// populated, `path` blank. UI surfaces a toast rather than
	/// opening a nonexistent tab.
	#[test]
	fn definition_outside_workspace_keeps_uri() {
		use std::str::FromStr;
		let tmp = tempfile::tempdir().unwrap();
		let outside = tmp.path().parent().unwrap().join("somewhere-else.rs");
		let uri_str = format!("file://{}", outside.display());
		let loc = lt::Location {
			uri: lt::Uri::from_str(&uri_str).unwrap(),
			range: lt::Range::default(),
		};
		let resp = definition_response(lt::GotoDefinitionResponse::Scalar(loc), tmp.path()).expect("location");
		assert!(resp.path.is_empty());
		assert!(resp.external_uri.starts_with("file://"));
	}

	#[test]
	fn hover_normalises_plaintext_markup() {
		let h = lt::Hover {
			contents: lt::HoverContents::Markup(lt::MarkupContent {
				kind: lt::MarkupKind::PlainText,
				value: "x: number".into(),
			}),
			range: None,
		};
		assert_eq!(hover(h).unwrap().contents, "```\nx: number\n```");
	}

	#[test]
	fn completion_item_emits_resolve_token_when_supported() {
		// `tsgo` / `rust-analyzer` ship empty `additional_text_edits`
		// in the initial completion response and gate the auto-import
		// line on `completionItem/resolve`. Our projection has to
		// emit a `resolve_token` (the full original item, JSON-
		// encoded) so the frontend can chase the resolve and round-
		// trip the exact item back to the server.
		let item = lt::CompletionItem {
			label: "useState".into(),
			kind: Some(lt::CompletionItemKind::FUNCTION),
			detail: Some("(initialState: S | (() => S)) => [S, Dispatch<SetStateAction<S>>]".into()),
			data: Some(serde_json::json!({ "exportName": "useState", "moduleSpecifier": "react" })),
			..Default::default()
		};
		let projected = completion_item_with_resolve(item, true);
		assert_eq!(projected.label, "useState");
		assert!(projected.additional_text_edits.is_empty());
		let token = projected
			.resolve_token
			.expect("resolve token must be present when supported");
		// Token round-trips back to a valid `lt::CompletionItem` —
		// the resolver hands this verbatim to the server.
		let restored: lt::CompletionItem = serde_json::from_str(&token).expect("token decodes to CompletionItem");
		assert_eq!(restored.label, "useState");
		assert_eq!(
			restored.data,
			Some(serde_json::json!({ "exportName": "useState", "moduleSpecifier": "react" }))
		);
	}

	#[test]
	fn completion_item_omits_resolve_token_when_unsupported() {
		// Servers that don't advertise `resolveProvider` get a
		// short-circuited surface — calling resolve on the item
		// would be a no-op, so we strip the token so the frontend
		// doesn't bother. Without this the frontend would chase a
		// pointless IPC for every accept on (e.g.) clangd builds
		// without `--background-index`.
		let item = lt::CompletionItem {
			label: "Foo".into(),
			..Default::default()
		};
		let projected = completion_item_with_resolve(item, false);
		assert!(projected.resolve_token.is_none());
	}

	#[test]
	fn completion_item_passes_through_additional_text_edits() {
		// The "rare" path: a server that pre-resolves the import
		// line (rust-analyzer with `imports.preferPrelude` does
		// this for some prelude items). The translator should pass
		// the edits through verbatim so the frontend can apply
		// them without an extra resolve round-trip.
		let item = lt::CompletionItem {
			label: "HashMap".into(),
			additional_text_edits: Some(vec![lt::TextEdit {
				range: lt::Range {
					start: lt::Position { line: 0, character: 0 },
					end: lt::Position { line: 0, character: 0 },
				},
				new_text: "use std::collections::HashMap;\n".into(),
			}]),
			..Default::default()
		};
		let projected = completion_item_with_resolve(item, true);
		assert_eq!(projected.additional_text_edits.len(), 1);
		assert_eq!(
			projected.additional_text_edits[0].new_text,
			"use std::collections::HashMap;\n"
		);
	}

	#[test]
	fn to_lsp_diagnostic_round_trips_code_and_severity() {
		// `textDocument/codeAction` requires the `context.diagnostics`
		// entries to match what the server originally emitted by
		// `(range, code)`; lose either and oxlint silently returns
		// no quickfixes. This test pins both fields.
		let d = mp::LspDiagnostic {
			range: mp::LspRange {
				start: mp::LspPosition { line: 5, character: 3 },
				end: mp::LspPosition { line: 5, character: 9 },
			},
			severity: mp::LspSeverity::Warning,
			message: "constant condition".into(),
			source: Some("oxc".into()),
			code: Some("eslint(no-constant-condition)".into()),
		};
		let lsp = to_lsp_diagnostic(&d);
		assert_eq!(lsp.severity, Some(lt::DiagnosticSeverity::WARNING));
		assert_eq!(lsp.message, "constant condition");
		assert_eq!(lsp.source.as_deref(), Some("oxc"));
		match lsp.code {
			Some(lt::NumberOrString::String(s)) => assert_eq!(s, "eslint(no-constant-condition)"),
			other => panic!("expected string code, got {other:?}"),
		}
		assert_eq!(lsp.range.start.line, 5);
		assert_eq!(lsp.range.end.character, 9);
	}

	#[test]
	fn code_actions_drops_command_only_entries() {
		// LSP's `Command` shape (no `edit`) is what oxlint ships for
		// its workspace-wide `oxc.fixAll` action and what tsgo ships
		// for "Organize imports". We don't run `workspace/executeCommand`
		// so rendering them as clickable-but-no-op tooltip entries is
		// worse than leaving them out.
		let resp = vec![lt::CodeActionOrCommand::Command(lt::Command {
			title: "Fix everything".into(),
			command: "oxc.fixAll".into(),
			arguments: None,
		})];
		let projected = code_actions(resp, "oxlint", std::path::Path::new("/tmp/root"));
		assert!(projected.is_empty(), "command-only entries dropped");
	}

	// `lt::Uri` uses interior mutability for ID interning so a
	// `HashMap<Uri, _>` trips `clippy::mutable_key_type`. Building
	// a `WorkspaceEdit::changes` map is the *only* way to feed
	// `code_actions` a translation-eligible edit; the lint doesn't
	// represent a real bug here (we never mutate the URIs after
	// insertion). Local allow so the surrounding production code
	// keeps the lint enabled.
	#[allow(clippy::mutable_key_type)]
	#[test]
	fn code_actions_keeps_quickfix_with_workspace_edit() {
		use std::str::FromStr;
		let tmp = tempfile::tempdir().unwrap();
		let file = tmp.path().join("src").join("a.ts");
		std::fs::create_dir_all(file.parent().unwrap()).unwrap();
		std::fs::write(&file, b"").unwrap();
		let uri_str = format!("file://{}", file.display());
		let mut changes = std::collections::HashMap::new();
		changes.insert(
			lt::Uri::from_str(&uri_str).unwrap(),
			vec![lt::TextEdit {
				range: lt::Range {
					start: lt::Position { line: 5, character: 0 },
					end: lt::Position { line: 5, character: 0 },
				},
				new_text: "// oxlint-disable-next-line no-constant-condition\n".into(),
			}],
		);
		let resp = vec![lt::CodeActionOrCommand::CodeAction(lt::CodeAction {
			title: "Disable no-constant-condition for this line".into(),
			kind: Some(lt::CodeActionKind::QUICKFIX),
			diagnostics: None,
			edit: Some(lt::WorkspaceEdit {
				changes: Some(changes),
				..Default::default()
			}),
			command: None,
			is_preferred: Some(false),
			disabled: None,
			data: None,
		})];
		let projected = code_actions(resp, "oxlint", tmp.path());
		assert_eq!(projected.len(), 1);
		assert_eq!(projected[0].title, "Disable no-constant-condition for this line");
		assert_eq!(projected[0].kind.as_deref(), Some("quickfix"));
		assert_eq!(projected[0].producer, "oxlint");
		assert_eq!(projected[0].edit.document_edits.len(), 1);
		assert_eq!(projected[0].edit.document_edits[0].path, "src/a.ts");
		assert_eq!(projected[0].edit.document_edits[0].edits.len(), 1);
		assert!(projected[0].edit.document_edits[0].edits[0]
			.new_text
			.contains("oxlint-disable-next-line"));
	}

	#[allow(clippy::mutable_key_type)] // see note on `code_actions_keeps_quickfix_with_workspace_edit`.
	#[test]
	fn code_actions_drops_quickfix_with_only_external_edits() {
		// A quickfix whose edits all target paths outside the
		// workspace root is unreachable for the frontend (the
		// open-buffer / fs-write paths only operate on workspace-
		// relative paths). Keeping it in the list would render a
		// clickable-but-silent tooltip entry — worse than dropping.
		use std::str::FromStr;
		let tmp = tempfile::tempdir().unwrap();
		let outside = tmp.path().parent().unwrap().join("elsewhere.ts");
		let uri_str = format!("file://{}", outside.display());
		let mut changes = std::collections::HashMap::new();
		changes.insert(
			lt::Uri::from_str(&uri_str).unwrap(),
			vec![lt::TextEdit {
				range: lt::Range::default(),
				new_text: "x".into(),
			}],
		);
		let resp = vec![lt::CodeActionOrCommand::CodeAction(lt::CodeAction {
			title: "Edit external file".into(),
			kind: Some(lt::CodeActionKind::QUICKFIX),
			diagnostics: None,
			edit: Some(lt::WorkspaceEdit {
				changes: Some(changes),
				..Default::default()
			}),
			command: None,
			is_preferred: None,
			disabled: None,
			data: None,
		})];
		let projected = code_actions(resp, "oxlint", tmp.path());
		assert!(projected.is_empty(), "external-only edits dropped");
	}

	#[test]
	fn completion_item_picks_replace_range_for_insert_replace_edit() {
		// We declared `insert_replace_support: false`, so well-
		// behaved servers send a plain `TextEdit`. A few (older
		// `tsserver` builds, some experimental servers) ignore
		// the capability and ship the dual-range shape anyway —
		// we honour the **replace** range, which matches what the
		// user means when they accept a completion that "rewrites"
		// the in-flight token.
		let item = lt::CompletionItem {
			label: "foo".into(),
			text_edit: Some(lt::CompletionTextEdit::InsertAndReplace(lt::InsertReplaceEdit {
				new_text: "fooBar".into(),
				insert: lt::Range {
					start: lt::Position { line: 0, character: 3 },
					end: lt::Position { line: 0, character: 3 },
				},
				replace: lt::Range {
					start: lt::Position { line: 0, character: 0 },
					end: lt::Position { line: 0, character: 6 },
				},
			})),
			..Default::default()
		};
		let projected = completion_item_with_resolve(item, false);
		let edit = projected.text_edit.expect("primary text edit projected");
		assert_eq!(edit.range.start.character, 0);
		assert_eq!(edit.range.end.character, 6);
		assert_eq!(edit.new_text, "fooBar");
	}
}
