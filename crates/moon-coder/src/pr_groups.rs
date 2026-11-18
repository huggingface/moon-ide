//! Sorting the inbox's pull requests into groups to review together
//! (ADR 0119): the prompt for the one model call, and reading its
//! answer back.

use std::collections::HashSet;
use std::fmt::Write as _;

use moon_protocol::inbox::{InboxGroup, InboxGroupAssignment, InboxGroupItem, InboxGroupRequest, InboxGrouping};
use serde::Deserialize;

/// Pull requests placed per call. More than this is a longer prompt
/// than the answer is worth; the rest wait for the next call.
pub const MAX_ITEMS: usize = 60;

const MAX_PATHS: usize = 8;
const MAX_NAME_CHARS: usize = 40;
const MAX_REASON_CHARS: usize = 120;

pub const SYSTEM_PROMPT: &str = "You sort a reviewer's open pull requests into groups they can go through together.\n\
\n\
Group by what the change is about: the same feature or area of the code, or the same kind of routine change \
(dependency bumps, CI, docs, generated files, mechanical refactors). Aim for few groups: around 3 to 8 for 30 pull \
requests. Put a pull request that fits nowhere into the closest group rather than alone, unless it is clearly a \
substantial change of its own. Never make a group called Other, Misc or similar. Who wrote a pull request is not a \
reason to group it.\n\
\n\
Each group has a `name` (2 to 4 words, sentence case, no emoji) and a `reason` (one short clause, under 12 words, \
saying what its pull requests have in common). When existing groups are listed, reuse one by its exact name if a \
pull request fits it; add a group only when none fits.\n\
\n\
Reply with JSON only, no prose and no code fence:\n\
{\"groups\":[{\"name\":\"...\",\"reason\":\"...\",\"prs\":[1,4]}]}\n\
`prs` holds the numbers from the list. Every pull request goes in exactly one group.";

fn size(item: &InboxGroupItem) -> String {
	match (item.additions, item.deletions) {
		(Some(added), Some(removed)) => format!(", +{added} -{removed}"),
		_ => String::new(),
	}
}

/// The user message: existing groups, then the first [`MAX_ITEMS`]
/// pull requests numbered from 1.
pub fn build_prompt(request: &InboxGroupRequest) -> String {
	let mut out = String::new();
	if !request.existing.is_empty() {
		out.push_str("Existing groups:\n");
		for group in &request.existing {
			let _ = writeln!(out, "- {}: {}", group.name, group.reason);
		}
		out.push('\n');
	}
	out.push_str("Pull requests:\n");
	for (index, item) in request.items.iter().take(MAX_ITEMS).enumerate() {
		let _ = write!(out, "{}. [{}] {}", index + 1, item.repo, item.title.trim());
		let author = item.author.as_deref().unwrap_or("unknown");
		let branch = item
			.head_ref
			.as_deref()
			.map(|b| format!(", branch {b}"))
			.unwrap_or_default();
		let _ = writeln!(out, " ({author}{branch}{})", size(item));
		if !item.paths.is_empty() {
			let shown: Vec<&str> = item.paths.iter().take(MAX_PATHS).map(String::as_str).collect();
			let more = item.paths.len().saturating_sub(MAX_PATHS);
			let tail = if more > 0 {
				format!(", and {more} more")
			} else {
				String::new()
			};
			let _ = writeln!(out, "   files: {}{tail}", shown.join(", "));
		}
		if !item.summary.is_empty() {
			let _ = writeln!(out, "   {}", item.summary);
		}
	}
	out
}

#[derive(Deserialize)]
struct RawAnswer {
	groups: Vec<RawGroup>,
}

#[derive(Deserialize)]
struct RawGroup {
	name: String,
	#[serde(default)]
	reason: String,
	#[serde(default)]
	prs: Vec<usize>,
}

fn clip(text: &str, max: usize) -> String {
	let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
	match line.char_indices().nth(max) {
		Some((cut, _)) => line[..cut].trim_end().to_owned(),
		None => line,
	}
}

/// Read the model's answer for `request`. Tolerates prose or a code
/// fence around the JSON, numbers out of range and a pull request
/// named twice (the first group keeps it). A group whose name matches
/// an existing one, whatever the case, is that group. Errors only
/// when there is no JSON of the expected shape at all.
pub fn parse_response(raw: &str, request: &InboxGroupRequest) -> Result<InboxGrouping, String> {
	let (Some(start), Some(end)) = (raw.find('{'), raw.rfind('}')) else {
		return Err("the model's answer has no JSON object".to_owned());
	};
	let answer: RawAnswer =
		serde_json::from_str(&raw[start..=end]).map_err(|err| format!("the model's answer: {err}"))?;

	let items: Vec<&InboxGroupItem> = request.items.iter().take(MAX_ITEMS).collect();
	let mut placed: HashSet<usize> = HashSet::new();
	let mut out = InboxGrouping::default();
	for raw_group in answer.groups {
		let name = clip(&raw_group.name, MAX_NAME_CHARS);
		if name.is_empty() {
			continue;
		}
		let known = request
			.existing
			.iter()
			.chain(out.groups.iter())
			.find(|group| group.name.eq_ignore_ascii_case(&name))
			.cloned();
		let group = known.unwrap_or(InboxGroup {
			name,
			reason: clip(&raw_group.reason, MAX_REASON_CHARS),
		});
		let mut used = false;
		for number in raw_group.prs {
			let Some(item) = number.checked_sub(1).and_then(|index| items.get(index)) else {
				continue;
			};
			if !placed.insert(number) {
				continue;
			}
			used = true;
			out.assignments.push(InboxGroupAssignment {
				id: item.id.clone(),
				group: group.name.clone(),
			});
		}
		if used && !out.groups.iter().any(|g| g.name == group.name) {
			out.groups.push(group);
		}
	}
	Ok(out)
}

#[cfg(test)]
mod tests {
	use super::*;

	fn item(id: &str, title: &str) -> InboxGroupItem {
		InboxGroupItem {
			id: id.to_owned(),
			repo: "acme/app".to_owned(),
			title: title.to_owned(),
			author: Some("alex".to_owned()),
			head_ref: Some("fix-it".to_owned()),
			additions: Some(12),
			deletions: Some(3),
			paths: (0..10).map(|i| format!("src/f{i}.rs")).collect(),
			summary: "Fixes it.".to_owned(),
		}
	}

	fn request(count: usize, existing: &[(&str, &str)]) -> InboxGroupRequest {
		InboxGroupRequest {
			items: (1..=count)
				.map(|i| item(&format!("acme/app#{i}"), &format!("PR {i}")))
				.collect(),
			existing: existing
				.iter()
				.map(|(name, reason)| InboxGroup {
					name: (*name).to_owned(),
					reason: (*reason).to_owned(),
				})
				.collect(),
		}
	}

	#[test]
	fn prompt_numbers_the_pull_requests_and_lists_existing_groups() {
		let prompt = build_prompt(&request(2, &[("Dependency bumps", "routine version updates")]));
		assert!(prompt.starts_with("Existing groups:\n- Dependency bumps: routine version updates\n\nPull requests:\n"));
		assert!(prompt.contains("1. [acme/app] PR 1 (alex, branch fix-it, +12 -3)\n"));
		assert!(prompt.contains(
			"   files: src/f0.rs, src/f1.rs, src/f2.rs, src/f3.rs, src/f4.rs, src/f5.rs, src/f6.rs, src/f7.rs, and 2 more\n"
		));
		assert!(prompt.contains("2. [acme/app] PR 2 "));
		assert!(!build_prompt(&request(1, &[])).contains("Existing groups"));
	}

	#[test]
	fn prompt_stops_at_the_cap() {
		let prompt = build_prompt(&request(MAX_ITEMS + 5, &[]));
		assert!(prompt.contains(&format!("{MAX_ITEMS}. [acme/app]")));
		assert!(!prompt.contains(&format!("{}. [acme/app]", MAX_ITEMS + 1)));
	}

	#[test]
	fn answer_becomes_groups_and_assignments() {
		let raw =
			"Here you go:\n```json\n{\"groups\":[{\"name\":\"Cursor fixes\",\"reason\":\"pagination  bugs\",\"prs\":[1,3]},\
			{\"name\":\"Docs\",\"reason\":\"readme only\",\"prs\":[2]}]}\n```";
		let grouping = parse_response(raw, &request(3, &[])).unwrap();
		assert_eq!(
			grouping.groups,
			[
				InboxGroup {
					name: "Cursor fixes".into(),
					reason: "pagination bugs".into()
				},
				InboxGroup {
					name: "Docs".into(),
					reason: "readme only".into()
				},
			]
		);
		let placed: Vec<_> = grouping
			.assignments
			.iter()
			.map(|a| (a.id.as_str(), a.group.as_str()))
			.collect();
		assert_eq!(
			placed,
			[
				("acme/app#1", "Cursor fixes"),
				("acme/app#3", "Cursor fixes"),
				("acme/app#2", "Docs")
			]
		);
	}

	#[test]
	fn existing_groups_are_reused_by_name_and_keep_their_reason() {
		let raw = r#"{"groups":[{"name":"dependency BUMPS","reason":"something else","prs":[1]}]}"#;
		let grouping = parse_response(raw, &request(1, &[("Dependency bumps", "routine version updates")])).unwrap();
		assert_eq!(grouping.assignments[0].group, "Dependency bumps");
		assert_eq!(grouping.groups[0].reason, "routine version updates");
	}

	#[test]
	fn bad_numbers_duplicates_and_empty_groups_are_dropped() {
		let raw = r#"{"groups":[
			{"name":"A","reason":"","prs":[0,1,9]},
			{"name":"B","reason":"","prs":[1,2]},
			{"name":"C","reason":"","prs":[]},
			{"name":"  ","reason":"","prs":[2]},
			{"name":"a","reason":"","prs":[2]}
		]}"#;
		let grouping = parse_response(raw, &request(3, &[])).unwrap();
		let placed: Vec<_> = grouping
			.assignments
			.iter()
			.map(|a| (a.id.as_str(), a.group.as_str()))
			.collect();
		assert_eq!(placed, [("acme/app#1", "A"), ("acme/app#2", "B")]);
		let names: Vec<_> = grouping.groups.iter().map(|g| g.name.as_str()).collect();
		assert_eq!(names, ["A", "B"]);
	}

	#[test]
	fn an_answer_without_json_is_an_error() {
		assert!(parse_response("I could not decide.", &request(1, &[])).is_err());
		assert!(parse_response(r#"{"teams":[]}"#, &request(1, &[])).is_err());
	}
}
