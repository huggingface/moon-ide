//! Translate between `lsp-types` (upstream LSP shapes) and
//! `moon_protocol::lsp` (what the UI consumes).
//!
//! Single place that knows the upstream crate exists. Keeps the rest
//! of `moon-core::lsp` dealing in either our types or raw JSON, never
//! both. When `lsp-types` changes an enum shape or adds a variant we
//! care about, this file is where the fix lands.

use lsp_types as lt;
use moon_protocol::lsp as mp;

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
	let documentation = item.documentation.map(|d| match d {
		lt::Documentation::String(s) => s,
		lt::Documentation::MarkupContent(m) => match m.kind {
			lt::MarkupKind::Markdown => m.value,
			lt::MarkupKind::PlainText => format!("```\n{}\n```", m.value),
		},
	});
	mp::LspCompletionItem {
		label: item.label,
		kind: item.kind.map(completion_kind),
		detail: item.detail,
		documentation,
		insert_text: item.insert_text,
		sort_text: item.sort_text,
		filter_text: item.filter_text,
	}
}

pub fn completion_response(resp: lt::CompletionResponse) -> mp::LspCompletionList {
	match resp {
		lt::CompletionResponse::Array(items) => mp::LspCompletionList {
			is_incomplete: false,
			items: items.into_iter().map(completion_item).collect(),
		},
		lt::CompletionResponse::List(list) => mp::LspCompletionList {
			is_incomplete: list.is_incomplete,
			items: list.items.into_iter().map(completion_item).collect(),
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
}
