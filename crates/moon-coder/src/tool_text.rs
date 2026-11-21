//! What the model reads of a tool's result.
//!
//! Results are JSON values: the panel renders them, and the session
//! file keeps them as JSON so a reopened transcript renders the same.
//! Sent to the model as JSON, though, a shell's output or a file's
//! text is one line with every newline and quote escaped, which costs
//! tokens on every later call of the turn and reads worse. So the
//! results that are mostly text (`bash`, `read_file`) go to the model
//! as that text, with their few other facts as bracketed lines; every
//! other result, and any of these in a shape not recognised here, as
//! JSON.

use serde_json::{Map, Value};

/// The text the model is given for `tool`'s result `value`.
pub fn for_model(tool: Option<&str>, value: &Value) -> String {
	let rendered = match (tool, value) {
		(Some("bash"), Value::Object(map)) => bash(map),
		(Some("read_file"), Value::Object(map)) => read_file(map),
		_ => None,
	};
	rendered.unwrap_or_else(|| value.to_string())
}

/// The same, from a result as the session file keeps it. Text that
/// isn't JSON (a result from before results were JSON) passes as is.
pub fn for_model_from_stored(tool: Option<&str>, stored: &str) -> String {
	match serde_json::from_str::<Value>(stored) {
		Ok(value) => for_model(tool, &value),
		Err(_) => stored.to_owned(),
	}
}

fn string<'a>(map: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
	map.get(key).and_then(Value::as_str)
}

fn flag(map: &Map<String, Value>, key: &str) -> bool {
	map.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn push_block(out: &mut String, text: &str) {
	if text.is_empty() {
		return;
	}
	out.push_str(text);
	if !text.ends_with('\n') {
		out.push('\n');
	}
}

/// A finished command: its output as printed, stderr under a marker,
/// then how it ended. The command itself is left out: the model has
/// it in its own call. A detached start is a handful of ids, and stays
/// JSON.
fn bash(map: &Map<String, Value>) -> Option<String> {
	const KNOWN: [&str; 9] = [
		"cmd",
		"target",
		"exit_code",
		"stdout",
		"stderr",
		"interrupted",
		"timed_out",
		"note",
		"truncated",
	];
	if map.keys().any(|key| !KNOWN.contains(&key.as_str())) {
		return None;
	}
	let stdout = string(map, "stdout")?;
	let stderr = string(map, "stderr")?;
	let mut out = String::new();
	push_block(&mut out, stdout);
	if !stderr.is_empty() {
		out.push_str("[stderr]\n");
		push_block(&mut out, stderr);
	}
	if let Some(note) = string(map, "note") {
		out.push_str(&format!("[note: {note}]\n"));
	}
	let ended = match map.get("exit_code").and_then(Value::as_i64) {
		Some(code) => format!("exit code {code}"),
		None if flag(map, "timed_out") => "timed out".to_owned(),
		None if flag(map, "interrupted") => "interrupted".to_owned(),
		None => "killed, no exit code".to_owned(),
	};
	let target = string(map, "target")
		.map(|target| format!(", on {target}"))
		.unwrap_or_default();
	out.push_str(&format!("[{ended}{target}]"));
	Some(out)
}

/// A file's numbered lines, then which lines they are when that isn't
/// the whole file, and whether the byte cap cut them short.
fn read_file(map: &Map<String, Value>) -> Option<String> {
	const KNOWN: [&str; 7] = [
		"path",
		"content",
		"start_line",
		"end_line",
		"total_lines",
		"truncated",
		"mtime_ms",
	];
	if map.keys().any(|key| !KNOWN.contains(&key.as_str())) {
		return None;
	}
	let content = string(map, "content")?;
	let total = map.get("total_lines").and_then(Value::as_u64)?;
	let start = map.get("start_line").and_then(Value::as_u64).unwrap_or(1);
	let end = map.get("end_line").and_then(Value::as_u64).unwrap_or(total);
	if total == 0 {
		return Some("[empty file]".to_owned());
	}
	let mut out = String::new();
	push_block(&mut out, content);
	if start > 1 || end < total {
		out.push_str(&format!("[lines {start}-{end} of {total}]\n"));
	}
	if flag(map, "truncated") {
		out.push_str("[truncated at the size cap: read a narrower range for the rest]\n");
	}
	if out.ends_with('\n') {
		out.pop();
	}
	Some(out)
}

#[cfg(test)]
mod tests {
	use super::*;
	use serde_json::json;

	#[test]
	fn a_command_reads_as_it_printed() {
		let result = json!({
			"cmd": "echo \"hi\"; ls nope",
			"target": "host",
			"exit_code": 2,
			"stdout": "hi\n",
			"stderr": "ls: nope: No such file\n",
		});
		assert_eq!(
			for_model(Some("bash"), &result),
			"hi\n[stderr]\nls: nope: No such file\n[exit code 2, on host]"
		);
		let quiet = json!({ "cmd": "true", "target": "container", "exit_code": 0, "stdout": "", "stderr": "" });
		assert_eq!(for_model(Some("bash"), &quiet), "[exit code 0, on container]");
	}

	#[test]
	fn a_command_cut_short_says_how() {
		let result = json!({
			"cmd": "sleep 999",
			"target": "host",
			"exit_code": null,
			"stdout": "partial",
			"stderr": "",
			"timed_out": true,
			"note": "killed after the 1000 ms timeout",
		});
		assert_eq!(
			for_model(Some("bash"), &result),
			"partial\n[note: killed after the 1000 ms timeout]\n[timed out, on host]"
		);
	}

	#[test]
	fn other_shapes_stay_json() {
		let detached = json!({ "detached": true, "id": "bg_1", "pid": 4, "cmd": "vite", "target": "host" });
		assert_eq!(for_model(Some("bash"), &detached), detached.to_string());
		let image = json!({ "path": "a.png", "mime": "image/png", "bytes": 10 });
		assert_eq!(for_model(Some("read_file"), &image), image.to_string());
		let other = json!({ "matches": [] });
		assert_eq!(for_model(Some("grep"), &other), other.to_string());
		assert_eq!(for_model(None, &other), other.to_string());
	}

	#[test]
	fn a_file_reads_as_its_lines() {
		let whole = json!({
			"path": "a.rs", "content": "1|fn a() {}\n2|", "start_line": 1, "end_line": 2,
			"total_lines": 2, "truncated": false, "mtime_ms": 5,
		});
		assert_eq!(for_model(Some("read_file"), &whole), "1|fn a() {}\n2|");
		let slice = json!({
			"path": "a.rs", "content": "10|x\n11|y\n", "start_line": 10, "end_line": 11,
			"total_lines": 90, "truncated": true, "mtime_ms": 5,
		});
		assert_eq!(
			for_model(Some("read_file"), &slice),
			"10|x\n11|y\n[lines 10-11 of 90]\n[truncated at the size cap: read a narrower range for the rest]"
		);
		let empty = json!({
			"path": "e", "content": "", "start_line": 1, "end_line": 1, "total_lines": 0, "truncated": false,
		});
		assert_eq!(for_model(Some("read_file"), &empty), "[empty file]");
	}

	#[test]
	fn stored_results_render_the_same_and_old_text_passes() {
		let result = json!({ "cmd": "ls", "target": "host", "exit_code": 0, "stdout": "a\n", "stderr": "" });
		assert_eq!(
			for_model_from_stored(Some("bash"), &result.to_string()),
			for_model(Some("bash"), &result)
		);
		assert_eq!(for_model_from_stored(Some("bash"), "not json"), "not json");
	}
}
