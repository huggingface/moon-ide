//! Turns delegated to the `claude` CLI, for sessions on a Claude plan
//! ([ADR 0131](../../../../specs/decisions/0131-claude-subscription-through-claude-code.md)).
//!
//! One `claude -p` process per turn, in streaming JSON mode, resumed
//! by Claude Code's own session id. It runs where the agent's
//! commands run (the workspace container when it is up), installed on
//! first use into a persisted directory there. Its output becomes
//! moon-ide events and session records, so the transcript renders like
//! any other; the turn loop around it (diffs, settle events, steers
//! queued after the turn) is the shared one in `spawn_turn_loop`.

use std::collections::HashMap;
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

use super::*;
use crate::inference::{AssistantResponse, FunctionCall, ImageAttachment, ToolCall};
use crate::mcp::{McpSpawnKind, McpSpawnTarget};
use crate::sessions::ClaudeCodeSession;
use moon_protocol::coder_models::{ChatGptUsage, ChatGptUsageWindow};

/// The persisted directory, as the container's shell sees it: the
/// workspace's `persist` volume survives the dev container being
/// recreated.
const CONTAINER_HOME: &str = "$HOME/.moon-persist/claude";
/// How long after a `result` the turn waits for Claude Code to pick
/// up a message written while it was busy, before calling it done.
const SETTLE_AFTER_RESULT: Duration = Duration::from_secs(3);
/// How long an interrupted `claude` gets to wind down before it is
/// killed.
const INTERRUPT_GRACE: Duration = Duration::from_secs(5);
const STEER_POLL: Duration = Duration::from_millis(400);

/// Install `claude` into `$P` (HOME pointed there, so the installer
/// and the updater write nowhere else), and refresh it at most daily
/// in the background. The updater needs no sign-in.
const ENSURE_INSTALLED: &str = r#"set -e
P="${MOON_CLAUDE_HOME}"
B="$P/.local/bin/claude"
mkdir -p "$P"
if [ ! -x "$B" ]; then
	echo "installing Claude Code into $P" >&2
	curl -fsSL https://claude.ai/install.sh | HOME="$P" bash >&2
	touch "$P/.updated"
elif [ -z "$(find "$P/.updated" -mmin -1440 2>/dev/null)" ]; then
	touch "$P/.updated"
	(HOME="$P" nohup "$B" update >/dev/null 2>&1 &)
fi
"#;

const INSTALLED: &str = r#"test -x "${MOON_CLAUDE_HOME}/.local/bin/claude""#;

/// Runs the CLI with its config (sessions included) in `$P`, leaving
/// the agent's own `HOME` alone.
const RUN: &str = r#"P="${MOON_CLAUDE_HOME}"
export CLAUDE_CONFIG_DIR="$P/config"
export DISABLE_AUTOUPDATER=1
exec "$P/.local/bin/claude" "$@"
"#;

/// Which Claude Code session, if any, a turn of `rt` runs in: the one
/// it is pinned to, or a new one when the active provider is a
/// Claude Code one.
pub(super) async fn delegated_provider(state: &Arc<CoderState>, rt: &Arc<SessionRuntime>) -> Option<String> {
	if let Some(pinned) = &rt.session.lock().await.header.claude_code {
		return Some(pinned.provider_id.clone());
	}
	state.models.read().await.active_claude_code().map(|p| p.id.clone())
}

/// One delegated turn. Sends the user messages not yet answered,
/// streams the reply into events and records, and returns when
/// Claude Code reports the turn finished.
pub(super) async fn run_claude_code_turn(
	state: &Arc<CoderState>,
	rt: &Arc<SessionRuntime>,
	folder_path: &Utf8Path,
	sink: &FolderEventSink,
	cancel: CancellationToken,
	provider_id: &str,
) -> Result<(), CoderError> {
	let model = {
		let models = state.models.read().await;
		let entry = models
			.provider(provider_id)
			.ok_or_else(|| CoderError::Internal("the Claude provider of this session was removed".into()))?;
		entry.standard_model.clone()
	};
	let token = state
		.provider_keys
		.get(provider_id)
		.ok_or_else(|| CoderError::Internal("Claude is not signed in. Sign in from the model settings.".into()))?;

	let routing_path = session_routing_path(state, rt, folder_path).await;
	let folder_entry = state
		.folder_entry_for(routing_path.as_str())
		.await
		.ok_or(CoderError::NoActiveFolder)?;
	let cx = ToolContext::new(folder_entry, CoderMode::Agent).with_force_host_bash(rt.force_host_bash.clone());
	let target = state.tools.mcp_spawn_target(&cx).await;
	let claude_home = match &target.kind {
		McpSpawnKind::Container { .. } => CONTAINER_HOME.to_owned(),
		McpSpawnKind::Host { .. } => state.tools.claude_home_on_host().to_string(),
	};

	ensure_installed(&target, &claude_home, sink, &cancel).await?;

	let (pinned, resuming, prompt, effort) = {
		let mut session = rt.session.lock().await;
		let resuming = session.header.claude_code.is_some();
		let pinned = session
			.header
			.claude_code
			.get_or_insert_with(|| ClaudeCodeSession {
				provider_id: provider_id.to_owned(),
				session_id: uuid::Uuid::new_v4().to_string(),
			})
			.clone();
		(
			pinned,
			resuming,
			unanswered_user_messages(&session.messages),
			session.header.reasoning_effort.clone(),
		)
	};
	if prompt.is_empty() {
		return Ok(());
	}
	if !resuming {
		persist_header(rt).await;
	}

	let mut args: Vec<String> = [
		"-p",
		"--input-format",
		"stream-json",
		"--output-format",
		"stream-json",
		"--verbose",
		"--include-partial-messages",
		// moon-ide's own agent asks before nothing either; the
		// container is the boundary (ADR 0131).
		"--permission-mode",
		"bypassPermissions",
	]
	.map(str::to_owned)
	.to_vec();
	if resuming {
		args.extend(["--resume".to_owned(), pinned.session_id.clone()]);
	} else {
		args.extend(["--session-id".to_owned(), pinned.session_id.clone()]);
	}
	if !model.trim().is_empty() {
		args.extend(["--model".to_owned(), model.trim().to_owned()]);
	}
	if let Some(effort) = effort.as_deref().and_then(claude_effort) {
		args.extend(["--effort".to_owned(), effort.to_owned()]);
	}

	let mut child = spawn(&target, &claude_home, RUN, &args, Some(&token))?;
	let mut stdin = child
		.stdin
		.take()
		.ok_or_else(|| CoderError::Internal("claude: stdin not piped".into()))?;
	let stdout = child
		.stdout
		.take()
		.ok_or_else(|| CoderError::Internal("claude: stdout not piped".into()))?;
	let stderr_tail = collect_stderr(child.stderr.take());

	let mut sent = 0usize;
	for message in &prompt {
		write_line(&mut stdin, &user_line(message)).await?;
		sent += 1;
	}

	let mut turn = TurnTranslator::new(state.clone(), rt.clone(), sink.clone(), target, provider_id.to_owned());
	let mut lines = BufReader::new(stdout).lines();
	let mut steer_poll = tokio::time::interval(STEER_POLL);
	steer_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
	let mut settle_deadline: Option<tokio::time::Instant> = None;
	let mut interrupted_at: Option<Instant> = None;

	let outcome = loop {
		let settle = async {
			match settle_deadline {
				Some(at) => tokio::time::sleep_until(at).await,
				None => std::future::pending().await,
			}
		};
		tokio::select! {
			line = lines.next_line() => {
				let line = match line {
					Ok(Some(line)) => line,
					Ok(None) => break turn.exited(interrupted_at.is_some(), &stderr_tail),
					Err(err) => break Err(CoderError::Internal(format!("claude: reading its output failed: {err}"))),
				};
				let Ok(message) = serde_json::from_str::<Value>(&line) else {
					continue;
				};
				// Any output means Claude Code is still at work.
				settle_deadline = None;
				if message.get("type").and_then(Value::as_str) == Some("control_request") {
					write_line(&mut stdin, &allow_control_request(&message)).await?;
					continue;
				}
				if let Some(result) = turn.handle(&message).await {
					if interrupted_at.is_some() {
						break Err(CoderError::Aborted);
					}
					if let Err(err) = result {
						break Err(err);
					}
					if turn.results >= sent {
						settle_deadline = Some(tokio::time::Instant::now() + SETTLE_AFTER_RESULT);
					}
				}
			}
			_ = settle => break Ok(()),
			_ = steer_poll.tick(), if interrupted_at.is_none() => {
				for message in drain_steers(rt, sink).await {
					write_line(&mut stdin, &user_line(&message)).await?;
					sent += 1;
					settle_deadline = None;
				}
			}
			_ = cancel.cancelled(), if interrupted_at.is_none() => {
				interrupted_at = Some(Instant::now());
				let _ = write_line(&mut stdin, &json!({
					"type": "control_request",
					"request_id": format!("moon-interrupt-{}", current_time_ms()),
					"request": { "subtype": "interrupt" },
				})).await;
			}
			_ = tokio::time::sleep(INTERRUPT_GRACE), if interrupted_at.is_some() => {
				break Err(CoderError::Aborted);
			}
		}
	};
	turn.finish_assistant().await;
	// A first turn that never got Claude Code to start its session
	// leaves nothing to resume: the next one creates it again.
	if !resuming && !turn.started_session {
		rt.session.lock().await.header.claude_code = None;
		persist_header(rt).await;
	}
	drop(stdin);
	if outcome.is_err() {
		let _ = child.start_kill();
	}
	let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
	outcome
}

async fn persist_header(rt: &Arc<SessionRuntime>) {
	let (dir, header) = {
		let session = rt.session.lock().await;
		(session.session_dir.clone(), session.header.clone())
	};
	let Some(dir) = dir else {
		return;
	};
	if let Err(err) = sessions::append_header(&dir, &header).await {
		tracing::warn!(error = %err, "could not record the Claude Code session");
	}
}

/// The user messages after the last assistant or tool message: what
/// Claude Code hasn't seen yet. A retry after a failed turn re-sends
/// the same ones.
fn unanswered_user_messages(messages: &[ChatMessage]) -> Vec<(String, Vec<ImageAttachment>)> {
	let mut out = Vec::new();
	for message in messages.iter().rev() {
		match message {
			ChatMessage::User { content, images } => out.push((content.clone(), images.clone())),
			ChatMessage::System { .. } => {}
			_ => break,
		}
	}
	out.reverse();
	out
}

/// Turns steers queued during the turn into user messages (events and
/// records, as between two rounds of moon-ide's own loop) and returns
/// them for Claude Code's stdin.
async fn drain_steers(rt: &Arc<SessionRuntime>, sink: &FolderEventSink) -> Vec<(String, Vec<ImageAttachment>)> {
	let before = {
		let session = rt.session.lock().await;
		if session.pending_steers.is_empty() {
			return Vec::new();
		}
		session.messages.len()
	};
	drain_pending_steers(rt, sink).await;
	let session = rt.session.lock().await;
	session.messages[before.min(session.messages.len())..]
		.iter()
		.filter_map(|m| match m {
			ChatMessage::User { content, images } => Some((content.clone(), images.clone())),
			_ => None,
		})
		.collect()
}

/// moon-ide's effort levels that Claude Code also has.
fn claude_effort(effort: &str) -> Option<&'static str> {
	match effort {
		"low" => Some("low"),
		"medium" => Some("medium"),
		"high" => Some("high"),
		"max" => Some("max"),
		_ => None,
	}
}

fn user_line((text, images): &(String, Vec<ImageAttachment>)) -> Value {
	let mut content: Vec<Value> = images
		.iter()
		.filter_map(|image| {
			let (_, data) = image.data_url.split_once(";base64,")?;
			Some(json!({
				"type": "image",
				"source": { "type": "base64", "media_type": image.mime, "data": data },
			}))
		})
		.collect();
	content.push(json!({ "type": "text", "text": text }));
	json!({
		"type": "user",
		"message": { "role": "user", "content": content },
		"parent_tool_use_id": null,
	})
}

/// Permissions are bypassed, so a prompt only comes from a tool that
/// asks regardless; let it through rather than stall the turn.
fn allow_control_request(message: &Value) -> Value {
	let request_id = message.get("request_id").cloned().unwrap_or(Value::Null);
	let request = message.get("request").cloned().unwrap_or(Value::Null);
	let response = match request.get("subtype").and_then(Value::as_str) {
		Some("can_use_tool") => json!({
			"behavior": "allow",
			"updatedInput": request.get("input").cloned().unwrap_or_else(|| json!({})),
		}),
		_ => json!({}),
	};
	json!({
		"type": "control_response",
		"response": { "subtype": "success", "request_id": request_id, "response": response },
	})
}

async fn write_line(stdin: &mut tokio::process::ChildStdin, value: &Value) -> Result<(), CoderError> {
	let mut line = value.to_string();
	line.push('\n');
	stdin
		.write_all(line.as_bytes())
		.await
		.map_err(|err| CoderError::Internal(format!("claude: writing to it failed: {err}")))?;
	stdin
		.flush()
		.await
		.map_err(|err| CoderError::Internal(format!("claude: writing to it failed: {err}")))
}

/// Starts `script` (with `args` as `$@`) where the agent's commands
/// run. The token reaches `claude` through the environment of
/// `docker exec` itself (`-e NAME` without a value), never on a
/// command line.
fn spawn(
	target: &McpSpawnTarget,
	claude_home: &str,
	script: &str,
	args: &[String],
	token: Option<&str>,
) -> Result<tokio::process::Child, CoderError> {
	let mut command = match &target.kind {
		McpSpawnKind::Host { cwd } => {
			let mut command = tokio::process::Command::new("sh");
			command.arg("-c").arg(script).arg("claude").args(args);
			command.current_dir(cwd).env("MOON_CLAUDE_HOME", claude_home);
			if let Some(token) = token {
				command.env("CLAUDE_CODE_OAUTH_TOKEN", token);
			}
			command
		}
		McpSpawnKind::Container { name, cwd } => {
			let mut command = tokio::process::Command::new("docker");
			command.arg("exec").arg("-i").arg("-w").arg(cwd);
			// Expanded by the container's shell (`$HOME` there).
			let script = format!("export MOON_CLAUDE_HOME=\"{claude_home}\"\n{script}");
			if let Some(token) = token {
				command.arg("-e").arg("CLAUDE_CODE_OAUTH_TOKEN");
				command.env("CLAUDE_CODE_OAUTH_TOKEN", token);
			}
			command
				.arg(name)
				.arg("sh")
				.arg("-c")
				.arg(script)
				.arg("claude")
				.args(args);
			command
		}
	};
	command
		.stdin(Stdio::piped())
		.stdout(Stdio::piped())
		.stderr(Stdio::piped())
		.kill_on_drop(true);
	command
		.spawn()
		.map_err(|err| CoderError::Internal(format!("could not start Claude Code: {err}")))
}

/// Installs (or refreshes) Claude Code. A first install takes a
/// minute or two, so it shows as a step of the turn.
async fn ensure_installed(
	target: &McpSpawnTarget,
	claude_home: &str,
	sink: &FolderEventSink,
	cancel: &CancellationToken,
) -> Result<(), CoderError> {
	let installed = run_script(target, claude_home, INSTALLED, cancel).await.is_ok();
	if installed {
		return run_script(target, claude_home, ENSURE_INSTALLED, cancel).await;
	}
	let id = new_message_id();
	let started = Instant::now();
	sink.send(CoderEvent::ToolCall {
		id: id.clone(),
		name: "bash".into(),
		args: json!({ "cmd": "curl -fsSL https://claude.ai/install.sh | bash  # Claude Code, first use in this workspace" }),
		started_at_ms: Some(current_time_ms()),
	});
	let result = run_script(target, claude_home, ENSURE_INSTALLED, cancel).await;
	sink.send(CoderEvent::ToolResult {
		id,
		result: Value::String(match &result {
			Ok(()) => "Claude Code installed.".into(),
			Err(err) => err.to_string(),
		}),
		is_error: result.is_err(),
		duration_ms: Some(started.elapsed().as_millis() as u64),
	});
	result
}

async fn run_script(
	target: &McpSpawnTarget,
	claude_home: &str,
	script: &str,
	cancel: &CancellationToken,
) -> Result<(), CoderError> {
	let mut child = spawn(target, claude_home, script, &[], None)?;
	drop(child.stdin.take());
	let stderr = collect_stderr(child.stderr.take());
	let status = tokio::select! {
		status = child.wait() => status.map_err(|err| CoderError::Internal(format!("Claude Code install: {err}")))?,
		_ = cancel.cancelled() => return Err(CoderError::Aborted),
	};
	if status.success() {
		return Ok(());
	}
	Err(CoderError::Internal(format!(
		"could not install Claude Code: {}",
		stderr.lock().map(|s| s.trim().to_owned()).unwrap_or_default()
	)))
}

/// Keeps the last few kilobytes of stderr for error messages.
fn collect_stderr(stderr: Option<tokio::process::ChildStderr>) -> Arc<std::sync::Mutex<String>> {
	let tail = Arc::new(std::sync::Mutex::new(String::new()));
	let Some(mut stderr) = stderr else {
		return tail;
	};
	let sink = tail.clone();
	tokio::spawn(async move {
		let mut buf = [0u8; 4096];
		while let Ok(n) = stderr.read(&mut buf).await {
			if n == 0 {
				break;
			}
			let Ok(mut tail) = sink.lock() else {
				break;
			};
			tail.push_str(&String::from_utf8_lossy(&buf[..n]));
			if tail.len() > 4000 {
				let cut = tail.len() - 4000;
				let cut = (cut..tail.len()).find(|i| tail.is_char_boundary(*i)).unwrap_or(cut);
				tail.drain(..cut);
			}
		}
	});
	tail
}

/// Claude Code's stream, translated into moon-ide events and records.
struct TurnTranslator {
	state: Arc<CoderState>,
	rt: Arc<SessionRuntime>,
	sink: FolderEventSink,
	target: McpSpawnTarget,
	provider_id: String,
	results: usize,
	/// Claude Code announced its session (`system/init`).
	started_session: bool,
	model: String,
	/// The assistant message being built, keyed by Claude's message
	/// id: Claude Code sends one `assistant` line per content block.
	current: Option<AssistantRow>,
	/// When each tool call started, for its duration.
	tool_started: HashMap<String, Instant>,
	/// Claude's tool names, by call id, for the tool records.
	tool_names: HashMap<String, String>,
}

struct AssistantRow {
	claude_id: String,
	id: String,
	started: bool,
	text: String,
	thinking: String,
	/// Text and thinking already shown from partial messages.
	streamed_text: bool,
	streamed_thinking: bool,
	tool_calls: Vec<ToolCall>,
}

impl TurnTranslator {
	fn new(
		state: Arc<CoderState>,
		rt: Arc<SessionRuntime>,
		sink: FolderEventSink,
		target: McpSpawnTarget,
		provider_id: String,
	) -> Self {
		Self {
			state,
			rt,
			sink,
			target,
			provider_id,
			results: 0,
			started_session: false,
			model: String::new(),
			current: None,
			tool_started: HashMap::new(),
			tool_names: HashMap::new(),
		}
	}

	/// Handles one output line; `Some` when it was a `result`.
	async fn handle(&mut self, message: &Value) -> Option<Result<(), CoderError>> {
		// Sub-agents' own traffic (Claude Code's `Task`) shows as the
		// tool call that started them, not inline.
		if message.get("parent_tool_use_id").is_some_and(|p| !p.is_null()) {
			return None;
		}
		match message.get("type").and_then(Value::as_str)? {
			"system" => {
				if message.get("subtype").and_then(Value::as_str) == Some("init") {
					self.started_session = true;
					if let Some(model) = message.get("model").and_then(Value::as_str) {
						self.model = model.to_owned();
					}
				}
				None
			}
			"stream_event" => {
				self.stream_event(message.get("event")?);
				None
			}
			"assistant" => {
				self.assistant(message.get("message")?).await;
				None
			}
			"user" => {
				self.tool_results(message.get("message")?).await;
				None
			}
			"rate_limit_event" => {
				if let Some(info) = message.get("rate_limit_info") {
					self.state.record_claude_rate_limit(&self.provider_id, info);
				}
				None
			}
			"result" => {
				self.finish_assistant().await;
				self.results += 1;
				Some(self.result(message).await)
			}
			_ => None,
		}
	}

	/// The row in progress (callers close a different message first).
	fn row(&mut self, claude_id: &str) -> &mut AssistantRow {
		self.current.get_or_insert_with(|| AssistantRow {
			claude_id: claude_id.to_owned(),
			id: new_message_id(),
			started: false,
			text: String::new(),
			thinking: String::new(),
			streamed_text: false,
			streamed_thinking: false,
			tool_calls: Vec::new(),
		})
	}

	fn stream_event(&mut self, event: &Value) {
		if event.get("type").and_then(Value::as_str) != Some("content_block_delta") {
			return;
		}
		let Some(delta) = event.get("delta") else {
			return;
		};
		let (is_text, chunk) = match delta.get("type").and_then(Value::as_str) {
			Some("text_delta") => (true, delta.get("text").and_then(Value::as_str)),
			Some("thinking_delta") => (false, delta.get("thinking").and_then(Value::as_str)),
			_ => return,
		};
		let Some(chunk) = chunk else {
			return;
		};
		let sink = self.sink.clone();
		// Partial messages carry no message id; they belong to the row
		// in progress, or open one that the full message then joins.
		let claude_id = self.current.as_ref().map(|r| r.claude_id.clone()).unwrap_or_default();
		let row = self.row(&claude_id);
		if !row.started {
			row.started = true;
			sink.send(CoderEvent::AssistantMessageStart { id: row.id.clone() });
		}
		if is_text {
			row.streamed_text = true;
			sink.send(CoderEvent::AssistantMessageDelta {
				id: row.id.clone(),
				delta: chunk.to_owned(),
			});
		} else {
			row.streamed_thinking = true;
			sink.send(CoderEvent::AssistantThinkingDelta {
				id: row.id.clone(),
				delta: chunk.to_owned(),
			});
		}
	}

	async fn assistant(&mut self, message: &Value) {
		let claude_id = message.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
		if let Some(model) = message.get("model").and_then(Value::as_str) {
			if !model.starts_with('<') {
				self.model = model.to_owned();
			}
		}
		// A different message: the previous one is complete.
		let other = self
			.current
			.as_ref()
			.is_some_and(|row| !row.claude_id.is_empty() && row.claude_id != claude_id);
		if other {
			self.finish_assistant().await;
		}
		let sink = self.sink.clone();
		let blocks: Vec<Value> = message
			.get("content")
			.and_then(Value::as_array)
			.cloned()
			.unwrap_or_default();
		let mut new_calls = Vec::new();
		let row = self.row(&claude_id);
		row.claude_id = claude_id;
		for block in &blocks {
			match block.get("type").and_then(Value::as_str) {
				Some("text") => {
					let text = block.get("text").and_then(Value::as_str).unwrap_or_default();
					if !row.streamed_text {
						if !row.started {
							row.started = true;
							sink.send(CoderEvent::AssistantMessageStart { id: row.id.clone() });
						}
						sink.send(CoderEvent::AssistantMessageDelta {
							id: row.id.clone(),
							delta: text.to_owned(),
						});
					}
					row.text.push_str(text);
				}
				Some("thinking") => {
					let thinking = block.get("thinking").and_then(Value::as_str).unwrap_or_default();
					row.thinking.push_str(thinking);
				}
				Some("tool_use") => {
					let id = block.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
					let name = block.get("name").and_then(Value::as_str).unwrap_or_default().to_owned();
					let input = block.get("input").cloned().unwrap_or_else(|| json!({}));
					let (moon_name, moon_args) = moon_tool(&name, &input);
					let args = moon_args.to_string();
					row.tool_calls.push(ToolCall {
						id: id.clone(),
						kind: "function".into(),
						function: FunctionCall {
							name: moon_name.clone(),
							arguments: args.clone(),
						},
						extra_content: None,
					});
					new_calls.push((id, name, moon_name, args));
				}
				_ => {}
			}
		}
		for (id, claude_name, moon_name, args) in new_calls {
			// Claude reports paths as it sees them; the IDE opens host paths.
			let args = self.target.host_paths(&args);
			if let Some(call) = self
				.current
				.as_mut()
				.and_then(|row| row.tool_calls.iter_mut().find(|call| call.id == id))
			{
				call.function.arguments = args.clone();
			}
			self.tool_started.insert(id.clone(), Instant::now());
			self.tool_names.insert(id.clone(), claude_name);
			self.sink.send(CoderEvent::ToolCall {
				id,
				name: moon_name,
				args: serde_json::from_str(&args).unwrap_or(Value::Null),
				started_at_ms: Some(current_time_ms()),
			});
		}
	}

	/// Closes the assistant message in progress: final event, record,
	/// and its place in the in-memory transcript.
	async fn finish_assistant(&mut self) {
		let Some(row) = self.current.take() else {
			return;
		};
		if row.text.is_empty() && row.thinking.is_empty() && row.tool_calls.is_empty() {
			return;
		}
		let thinking = (!row.thinking.is_empty()).then(|| row.thinking.clone());
		if row.started || !row.text.is_empty() || thinking.is_some() {
			if !row.started {
				self.sink.send(CoderEvent::AssistantMessageStart { id: row.id.clone() });
			}
			if !row.streamed_thinking {
				if let Some(thinking) = &thinking {
					self.sink.send(CoderEvent::AssistantThinkingDelta {
						id: row.id.clone(),
						delta: thinking.clone(),
					});
				}
			}
			self.sink.send(CoderEvent::AssistantMessageEnd {
				id: row.id.clone(),
				text: row.text.clone(),
				thinking: thinking.clone(),
				created_at_ms: Some(current_time_ms()),
			});
		}
		let response = AssistantResponse {
			content: (!row.text.is_empty()).then(|| row.text.clone()),
			thinking,
			thinking_blocks: Vec::new(),
			tool_calls: row.tool_calls.clone(),
			usage: None,
			stop_reason: None,
		};
		persist_assistant_record(&self.rt, &response, Some(self.pi_model())).await;
		self.rt.session.lock().await.messages.push(ChatMessage::Assistant {
			content: response.content.clone(),
			thinking_blocks: Vec::new(),
			tool_calls: row.tool_calls,
		});
	}

	async fn tool_results(&mut self, message: &Value) {
		let Some(blocks) = message.get("content").and_then(Value::as_array) else {
			return;
		};
		let results: Vec<&Value> = blocks
			.iter()
			.filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
			.collect();
		if results.is_empty() {
			return;
		}
		self.finish_assistant().await;
		for block in results {
			let id = block
				.get("tool_use_id")
				.and_then(Value::as_str)
				.unwrap_or_default()
				.to_owned();
			let is_error = block.get("is_error").and_then(Value::as_bool).unwrap_or(false);
			let text = self.target.host_paths(&tool_result_text(block.get("content")));
			let duration_ms = self.tool_started.remove(&id).map(|t| t.elapsed().as_millis() as u64);
			let claude_name = self.tool_names.remove(&id).unwrap_or_default();
			let (moon_name, _) = moon_tool(&claude_name, &Value::Null);
			self.sink.send(CoderEvent::ToolResult {
				id: id.clone(),
				result: Value::String(text.clone()),
				is_error,
				duration_ms,
			});
			persist_tool_record(&self.rt, &id, &moon_name, &text, duration_ms, &[]).await;
			self.rt.session.lock().await.messages.push(ChatMessage::Tool {
				tool_call_id: id,
				tool_name: Some(moon_name),
				content: text,
				images: Vec::new(),
			});
		}
	}

	async fn result(&mut self, message: &Value) -> Result<(), CoderError> {
		if let Some(usage) = message.get("usage") {
			self.usage(usage, message.get("modelUsage")).await;
		}
		let failed = message.get("is_error").and_then(Value::as_bool).unwrap_or(false)
			|| message
				.get("subtype")
				.and_then(Value::as_str)
				.is_some_and(|s| s != "success");
		if !failed {
			return Ok(());
		}
		let text = message
			.get("result")
			.and_then(Value::as_str)
			.filter(|t| !t.trim().is_empty())
			.map(str::to_owned)
			.or_else(|| {
				message
					.get("errors")
					.and_then(Value::as_array)
					.map(|errors| errors.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("; "))
			})
			.filter(|t| !t.trim().is_empty())
			.unwrap_or_else(|| {
				format!(
					"Claude Code stopped ({})",
					message
						.get("terminal_reason")
						.or_else(|| message.get("subtype"))
						.and_then(Value::as_str)
						.unwrap_or("error")
				)
			});
		Err(CoderError::Internal(text))
	}

	async fn usage(&mut self, usage: &Value, model_usage: Option<&Value>) {
		let n = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0) as u32;
		let cache_read = n("cache_read_input_tokens");
		let cache_creation = n("cache_creation_input_tokens");
		let prompt = n("input_tokens") + cache_read + cache_creation;
		let completion = n("output_tokens");
		let context_window = model_usage
			.and_then(Value::as_object)
			.and_then(|m| m.values().filter_map(|v| v.get("contextWindow")?.as_u64()).max())
			.unwrap_or(200_000) as u32;
		let response = AssistantResponse {
			content: None,
			thinking: None,
			thinking_blocks: Vec::new(),
			tool_calls: Vec::new(),
			stop_reason: None,
			usage: Some(crate::inference::TokenUsage {
				prompt_tokens: prompt,
				completion_tokens: completion,
				total_tokens: prompt + completion,
				cache_read_input_tokens: cache_read,
				cache_creation_input_tokens: cache_creation,
			}),
		};
		persist_usage_record(&self.rt, &response).await;
		self.sink.send(CoderEvent::TokenUsage {
			prompt_tokens: prompt,
			completion_tokens: completion,
			total_tokens: prompt + completion,
			context_window,
			source: TokenUsageSource::Provider,
			session_cache_hits: 0,
			session_requests: 0,
			model: self.model.clone(),
			cache_read_tokens: cache_read,
			cache_creation_tokens: cache_creation,
		});
	}

	fn pi_model(&self) -> String {
		if self.model.is_empty() {
			"claude-code".to_owned()
		} else {
			self.model.clone()
		}
	}

	/// The process ended without the result that ends a turn.
	fn exited(&self, interrupted: bool, stderr: &std::sync::Mutex<String>) -> Result<(), CoderError> {
		if interrupted {
			return Err(CoderError::Aborted);
		}
		if self.results > 0 {
			return Ok(());
		}
		let tail = stderr.lock().map(|s| s.trim().to_owned()).unwrap_or_default();
		Err(CoderError::Internal(if tail.is_empty() {
			"Claude Code exited without answering".to_owned()
		} else {
			format!("Claude Code exited: {tail}")
		}))
	}
}

impl CoderState {
	/// Folds one `rate_limit_event` into the provider's plan usage:
	/// the five-hour window as the short one, the weekly as the long
	/// one, in the ChatGPT plan's shape so the same gauge shows both.
	pub(super) fn record_claude_rate_limit(&self, provider_id: &str, info: &Value) {
		let Ok(mut all) = self.claude_usage.lock() else {
			return;
		};
		let usage = all.entry(provider_id.to_owned()).or_insert(ChatGptUsage {
			plan_type: None,
			primary: None,
			secondary: None,
			limit_reached: false,
			credits_balance: None,
			resets_available: None,
		});
		let window_seconds = match info.get("rateLimitType").and_then(Value::as_str) {
			Some("five_hour") => 5 * 3600,
			Some("seven_day") => 7 * 24 * 3600,
			_ => 0,
		};
		let rejected = info.get("status").and_then(Value::as_str) == Some("rejected");
		if window_seconds > 0 {
			let window = ChatGptUsageWindow {
				used_percent: info
					.get("utilization")
					.and_then(Value::as_f64)
					.map_or(if rejected { 100.0 } else { 0.0 }, |u| (u * 100.0).clamp(0.0, 100.0)),
				window_seconds: Some(window_seconds),
				resets_at: info.get("resetsAt").and_then(Value::as_i64),
			};
			if window_seconds == 5 * 3600 {
				usage.primary = Some(window);
			} else {
				usage.secondary = Some(window);
			}
		}
		usage.limit_reached = rejected;
		if info.get("isUsingOverage").and_then(Value::as_bool) == Some(true) {
			usage.credits_balance = Some("using extra usage".to_owned());
		}
	}

	pub(super) fn claude_usage(&self, provider_id: &str) -> Option<ChatGptUsage> {
		self.claude_usage.lock().ok()?.get(provider_id).cloned()
	}
}

/// moon-ide's name and argument shape for Claude Code's common tools,
/// so they render like the native ones; others keep their own.
fn moon_tool(name: &str, input: &Value) -> (String, Value) {
	let s = |key: &str| input.get(key).cloned().unwrap_or(Value::Null);
	match name {
		"Bash" => ("bash".into(), json!({ "cmd": s("command") })),
		"Read" => {
			let mut args = json!({ "path": s("file_path") });
			if let Some(offset) = input.get("offset").and_then(Value::as_u64) {
				args["start_line"] = json!(offset);
				if let Some(limit) = input.get("limit").and_then(Value::as_u64) {
					args["end_line"] = json!(offset + limit.saturating_sub(1));
				}
			}
			("read_file".into(), args)
		}
		"Write" => (
			"write_file".into(),
			json!({ "path": s("file_path"), "content": s("content") }),
		),
		"Edit" => (
			"edit_file".into(),
			json!({ "path": s("file_path"), "find": s("old_string"), "replace": s("new_string") }),
		),
		"Grep" => ("grep".into(), json!({ "pattern": s("pattern"), "path": s("path") })),
		"WebSearch" => ("web_search".into(), json!({ "query": s("query") })),
		"WebFetch" => ("web_fetch".into(), json!({ "url": s("url") })),
		_ => (name.to_owned(), input.clone()),
	}
}

fn tool_result_text(content: Option<&Value>) -> String {
	match content {
		Some(Value::String(text)) => text.clone(),
		Some(Value::Array(blocks)) => blocks
			.iter()
			.filter_map(|b| b.get("text").and_then(Value::as_str))
			.collect::<Vec<_>>()
			.join("\n"),
		Some(other) => other.to_string(),
		None => String::new(),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// A stand-in `claude` that logs how it was started and replays
	/// one recorded turn: streamed text, a tool call and its result,
	/// a final answer, a rate-limit report and the result.
	const FAKE_CLAUDE: &str = r#"#!/bin/sh
[ "$1" = update ] && exit 0
echo "$*" >> "$MOON_CLAUDE_HOME/args.log"
echo "$CLAUDE_CODE_OAUTH_TOKEN" >> "$MOON_CLAUDE_HOME/token.log"
read -r line
echo "$line" >> "$MOON_CLAUDE_HOME/stdin.log"
cat <<'END'
{"type":"system","subtype":"init","session_id":"s","model":"claude-opus-5-5","tools":[]}
{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Let me "}}}
{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"look."}}}
{"type":"assistant","message":{"id":"msg_1","model":"claude-opus-5-5","content":[{"type":"text","text":"Let me look."}]},"parent_tool_use_id":null}
{"type":"assistant","message":{"id":"msg_1","model":"claude-opus-5-5","content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"ls"}}]},"parent_tool_use_id":null}
{"type":"assistant","message":{"id":"msg_sub","content":[{"type":"text","text":"sub-agent chatter"}]},"parent_tool_use_id":"toolu_9"}
{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"a.txt\nb.txt","is_error":false}]},"parent_tool_use_id":null}
{"type":"assistant","message":{"id":"msg_2","model":"claude-opus-5-5","content":[{"type":"text","text":"Two files."}]},"parent_tool_use_id":null}
{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","rateLimitType":"five_hour","utilization":0.42,"resetsAt":1800000000}}
{"type":"result","subtype":"success","is_error":false,"result":"Two files.","usage":{"input_tokens":10,"output_tokens":20,"cache_read_input_tokens":1000,"cache_creation_input_tokens":5},"modelUsage":{"claude-opus-5-5":{"contextWindow":1000000}}}
END
cat > /dev/null
"#;

	#[tokio::test]
	async fn a_turn_runs_in_claude_code_and_reads_like_any_other() {
		keyring::set_default_credential_builder(keyring::mock::default_credential_builder());
		let folder_dir = tempfile::TempDir::new().unwrap();
		let folder = Utf8PathBuf::from_path_buf(folder_dir.path().canonicalize().unwrap()).unwrap();
		let registry = Arc::new(WorkspaceRegistry::new("test-workspace".into()));
		registry.add_folder(folder.clone()).await.unwrap();
		let data_dir = tempfile::TempDir::new().unwrap();
		let data = Utf8PathBuf::from_path_buf(data_dir.path().to_path_buf()).unwrap();
		let claude_home = data.join("claude");
		let bin = claude_home.join(".local/bin");
		std::fs::create_dir_all(&bin).unwrap();
		std::fs::write(bin.join("claude"), FAKE_CLAUDE).unwrap();
		{
			use std::os::unix::fs::PermissionsExt;
			std::fs::set_permissions(bin.join("claude"), std::fs::Permissions::from_mode(0o755)).unwrap();
		}
		std::fs::write(claude_home.join(".updated"), "").unwrap();

		let mut models = CoderModels::default();
		models.providers.push(moon_protocol::coder_models::CoderProviderConfig {
			id: "prov-claude".into(),
			label: "Claude".into(),
			kind: moon_protocol::coder_models::ProviderKind::ClaudeCode,
			base_url: String::new(),
			standard_model: "opus".into(),
			cheap_model: String::new(),
			payload_cap_mb: None,
			has_api_key: false,
		});
		models.active_provider = Some("prov-claude".into());
		let coder = CoderHandle::new(
			registry,
			data.join("workspaces"),
			data.join("coder-sessions"),
			data.join("folder-summaries"),
			models,
			Arc::new(moon_terminal::TerminalRegistry::default()),
		)
		.unwrap();
		coder.set_provider_api_key("prov-claude", "sk-ant-oat01-test").unwrap();
		let mut events = coder.subscribe();

		let session = coder.new_session().await.unwrap();
		coder.send("what is here?".into(), Vec::new()).await.unwrap();
		let mut seen = Vec::new();
		let settled = tokio::time::timeout(Duration::from_secs(20), async {
			loop {
				let envelope = events.recv().await.unwrap();
				let done = matches!(
					envelope.event,
					CoderEvent::TurnComplete | CoderEvent::Error { .. } | CoderEvent::Aborted
				);
				seen.push(envelope.event);
				if done {
					return;
				}
			}
		})
		.await;
		assert!(settled.is_ok(), "the turn settles: {seen:?}");
		assert!(
			matches!(seen.last(), Some(CoderEvent::TurnComplete)),
			"events: {seen:?}"
		);

		let tool_call = seen.iter().find_map(|e| match e {
			CoderEvent::ToolCall { name, args, .. } => Some((name.clone(), args.clone())),
			_ => None,
		});
		assert_eq!(tool_call, Some(("bash".into(), json!({ "cmd": "ls" }))));
		let ends: Vec<String> = seen
			.iter()
			.filter_map(|e| match e {
				CoderEvent::AssistantMessageEnd { text, .. } => Some(text.clone()),
				_ => None,
			})
			.collect();
		assert_eq!(ends, ["Let me look.", "Two files."], "sub-agent text stays out");
		assert!(seen.iter().any(|e| matches!(
			e,
			CoderEvent::TokenUsage {
				prompt_tokens: 1015,
				context_window: 1_000_000,
				..
			}
		)));

		let usage = coder.chatgpt_usage("prov-claude").await.unwrap();
		assert_eq!(usage.primary.map(|w| w.used_percent.round()), Some(42.0));

		let read = |name: &str| std::fs::read_to_string(claude_home.join(name)).unwrap_or_default();
		assert!(
			read("args.log").contains("--session-id"),
			"first turn creates: {}",
			read("args.log")
		);
		assert!(read("args.log").contains("--model opus"));
		assert_eq!(read("token.log").trim(), "sk-ant-oat01-test");
		assert!(read("stdin.log").contains("what is here?"));

		// The second turn resumes the session the first one created,
		// sending only the new message.
		coder.send("and now?".into(), Vec::new()).await.unwrap();
		let second = tokio::time::timeout(Duration::from_secs(20), async {
			loop {
				if matches!(events.recv().await.unwrap().event, CoderEvent::TurnComplete) {
					break;
				}
			}
		})
		.await;
		assert!(second.is_ok(), "the second turn settles");
		let args = read("args.log");
		let second_args = args.lines().nth(1).unwrap_or_default();
		assert!(second_args.contains("--resume"), "second turn resumes: {second_args}");
		let stdin = read("stdin.log");
		let second_input = stdin.lines().nth(1).unwrap_or_default();
		assert!(second_input.contains("and now?") && !second_input.contains("what is here?"));

		// Rewinding is refused rather than silently diverging.
		let err = coder.revert_to_message_in(&session.id, 0).await.unwrap_err();
		assert!(err.to_string().contains("Claude Code"), "{err}");
	}

	/// Sets up a workspace whose active provider is a Claude plan, with
	/// `script` standing in for `claude`.
	async fn claude_workspace(script: &str) -> (CoderHandle, Utf8PathBuf, Vec<tempfile::TempDir>) {
		keyring::set_default_credential_builder(keyring::mock::default_credential_builder());
		let folder_dir = tempfile::TempDir::new().unwrap();
		let folder = Utf8PathBuf::from_path_buf(folder_dir.path().canonicalize().unwrap()).unwrap();
		let registry = Arc::new(WorkspaceRegistry::new("test-workspace".into()));
		registry.add_folder(folder.clone()).await.unwrap();
		let data_dir = tempfile::TempDir::new().unwrap();
		let data = Utf8PathBuf::from_path_buf(data_dir.path().to_path_buf()).unwrap();
		let claude_home = data.join("claude");
		let bin = claude_home.join(".local/bin");
		std::fs::create_dir_all(&bin).unwrap();
		std::fs::write(bin.join("claude"), script).unwrap();
		{
			use std::os::unix::fs::PermissionsExt;
			std::fs::set_permissions(bin.join("claude"), std::fs::Permissions::from_mode(0o755)).unwrap();
		}
		std::fs::write(claude_home.join(".updated"), "").unwrap();
		let mut models = CoderModels::default();
		models.providers.push(moon_protocol::coder_models::CoderProviderConfig {
			id: "prov-claude".into(),
			label: "Claude".into(),
			kind: moon_protocol::coder_models::ProviderKind::ClaudeCode,
			base_url: String::new(),
			standard_model: "opus".into(),
			cheap_model: String::new(),
			payload_cap_mb: None,
			has_api_key: false,
		});
		models.active_provider = Some("prov-claude".into());
		let coder = CoderHandle::new(
			registry,
			data.join("workspaces"),
			data.join("coder-sessions"),
			data.join("folder-summaries"),
			models,
			Arc::new(moon_terminal::TerminalRegistry::default()),
		)
		.unwrap();
		coder.set_provider_api_key("prov-claude", "sk-ant-oat01-test").unwrap();
		coder.new_session().await.unwrap();
		(coder, claude_home, vec![folder_dir, data_dir])
	}

	async fn wait_for(
		events: &mut broadcast::Receiver<CoderEventEnvelope>,
		pred: impl Fn(&CoderEvent) -> bool,
	) -> Vec<CoderEvent> {
		let mut seen = Vec::new();
		let found = tokio::time::timeout(Duration::from_secs(20), async {
			loop {
				let event = events.recv().await.unwrap().event;
				let hit = pred(&event);
				seen.push(event);
				if hit {
					return;
				}
			}
		})
		.await;
		assert!(found.is_ok(), "never saw the awaited event: {seen:?}");
		seen
	}

	#[tokio::test]
	async fn stop_interrupts_claude_code() {
		// Answers nothing until told to stop, then reports the
		// interrupted turn the way Claude Code does.
		let script = r#"#!/bin/sh
read -r line
echo '{"type":"system","subtype":"init","session_id":"s","model":"m","tools":[]}'
echo '{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"working"}}}'
while read -r line; do
	case "$line" in
		*interrupt*)
			echo "$line" >> "$MOON_CLAUDE_HOME/interrupts.log"
			echo '{"type":"result","subtype":"error_during_execution","is_error":true,"result":""}'
			exit 0;;
	esac
done
"#;
		let (coder, claude_home, _dirs) = claude_workspace(script).await;
		let mut events = coder.subscribe();
		coder.send("long job".into(), Vec::new()).await.unwrap();
		wait_for(&mut events, |e| matches!(e, CoderEvent::AssistantMessageDelta { .. })).await;
		coder.abort().await;
		let seen = wait_for(&mut events, |e| {
			matches!(
				e,
				CoderEvent::Aborted | CoderEvent::TurnComplete | CoderEvent::Error { .. }
			)
		})
		.await;
		assert!(matches!(seen.last(), Some(CoderEvent::Aborted)), "{seen:?}");
		let interrupts = std::fs::read_to_string(claude_home.join("interrupts.log")).unwrap_or_default();
		assert!(interrupts.contains("\"subtype\":\"interrupt\""), "{interrupts}");
	}

	#[tokio::test]
	async fn a_message_sent_mid_turn_reaches_claude_code_in_the_same_turn() {
		// Waits for a second message before answering both.
		let script = r#"#!/bin/sh
read -r first
echo '{"type":"system","subtype":"init","session_id":"s","model":"m","tools":[]}'
read -r second
echo "$second" >> "$MOON_CLAUDE_HOME/stdin.log"
echo '{"type":"assistant","message":{"id":"msg_1","content":[{"type":"text","text":"both seen"}]},"parent_tool_use_id":null}'
echo '{"type":"result","subtype":"success","is_error":false,"result":"both seen"}'
echo '{"type":"result","subtype":"success","is_error":false,"result":"both seen"}'
cat > /dev/null
"#;
		let (coder, claude_home, _dirs) = claude_workspace(script).await;
		let mut events = coder.subscribe();
		coder.send("first".into(), Vec::new()).await.unwrap();
		// Let the turn start before steering it.
		tokio::time::sleep(Duration::from_millis(500)).await;
		coder.send("also this".into(), Vec::new()).await.unwrap();
		let seen = wait_for(&mut events, |e| {
			matches!(
				e,
				CoderEvent::TurnComplete | CoderEvent::Error { .. } | CoderEvent::Aborted
			)
		})
		.await;
		assert!(matches!(seen.last(), Some(CoderEvent::TurnComplete)), "{seen:?}");
		assert!(seen.iter().any(|e| matches!(e, CoderEvent::SteerDrained { .. })));
		let stdin = std::fs::read_to_string(claude_home.join("stdin.log")).unwrap_or_default();
		assert!(stdin.contains("also this"), "{stdin}");
	}

	#[test]
	fn common_tools_render_as_moon_tools() {
		let (name, args) = moon_tool(
			"Edit",
			&json!({ "file_path": "/w/a.rs", "old_string": "a", "new_string": "b" }),
		);
		assert_eq!(name, "edit_file");
		assert_eq!(args, json!({ "path": "/w/a.rs", "find": "a", "replace": "b" }));

		let (name, args) = moon_tool("Read", &json!({ "file_path": "/w/a.rs", "offset": 10, "limit": 5 }));
		assert_eq!(name, "read_file");
		assert_eq!(args, json!({ "path": "/w/a.rs", "start_line": 10, "end_line": 14 }));

		let (name, args) = moon_tool("TodoWrite", &json!({ "todos": [] }));
		assert_eq!((name.as_str(), args), ("TodoWrite", json!({ "todos": [] })));
	}

	#[test]
	fn only_unanswered_user_messages_are_sent() {
		let messages = vec![
			ChatMessage::user("first"),
			ChatMessage::Assistant {
				content: Some("ok".into()),
				thinking_blocks: Vec::new(),
				tool_calls: Vec::new(),
			},
			ChatMessage::user("second"),
			ChatMessage::user("third"),
		];
		let texts: Vec<String> = unanswered_user_messages(&messages)
			.into_iter()
			.map(|(t, _)| t)
			.collect();
		assert_eq!(texts, ["second", "third"]);
	}

	#[test]
	fn images_go_before_the_text() {
		let line = user_line(&(
			"look".into(),
			vec![ImageAttachment {
				data_url: "data:image/png;base64,AAAA".into(),
				mime: "image/png".into(),
			}],
		));
		let content = &line["message"]["content"];
		assert_eq!(content[0]["source"]["data"], "AAAA");
		assert_eq!(content[1], json!({ "type": "text", "text": "look" }));
	}

	#[test]
	fn tool_permission_prompts_are_allowed() {
		let reply = allow_control_request(&json!({
			"type": "control_request",
			"request_id": "r1",
			"request": { "subtype": "can_use_tool", "tool_name": "Bash", "input": { "command": "ls" } },
		}));
		assert_eq!(reply["response"]["request_id"], "r1");
		assert_eq!(reply["response"]["response"]["behavior"], "allow");
		assert_eq!(reply["response"]["response"]["updatedInput"]["command"], "ls");
	}
}
