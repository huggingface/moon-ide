//! Minimal MCP (Model Context Protocol) client — stdio transport
//! only — plus the curated preset registry and the per-workspace
//! config merge helpers.
//!
//! Design (ADR 0033): the coder does **not** advertise every tool
//! of every enabled MCP server to the model. Instead two meta-tools
//! (`mcp_list_tools` / `mcp_call`, defined in
//! [`crate::tools::ToolRegistry`]) carry the enabled-server list in
//! their descriptions; per-server tool schemas only enter the
//! context when the model asks for them. That keeps the tool list
//! stable and the token cost proportional to actual use.
//!
//! The client is hand-rolled rather than pulling in an MCP SDK:
//! stdio MCP is newline-delimited JSON-RPC 2.0 with a three-step
//! handshake — the same "few hundred lines around a parser" bet as
//! the inference client (ADR 0010).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use moon_protocol::coder_mcp::{CoderMcpWorkspaceConfig, McpRunTarget, McpServerConfig, McpServerStatus};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::{oneshot, Mutex};
use tokio_util::sync::CancellationToken;

use crate::error::CoderError;

/// Protocol revision we ask for at `initialize`. Servers negotiate
/// down when they're older; we don't gate on the reply.
const MCP_PROTOCOL_VERSION: &str = "2025-06-18";

/// Handshake + `tools/list` budget. Generous because `npx`-shaped
/// servers may download their package on first spawn.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(60);

/// Per-`tools/call` budget. Browser automation steps are slow but
/// bounded; a genuinely hung server shouldn't park a turn forever.
const CALL_TIMEOUT: Duration = Duration::from_secs(120);

/// The curated preset list. Bootstrap posture: playwright is the
/// server the team actually asked for; further presets are added
/// when someone needs them, not preemptively.
pub fn preset_servers() -> Vec<McpServerConfig> {
	vec![McpServerConfig {
		id: "playwright".into(),
		label: "Playwright".into(),
		command: "npx".into(),
		args: vec!["-y".into(), "@playwright/mcp@latest".into()],
		// Host by default: driving a browser needs one installed,
		// and moon-base doesn't ship browsers.
		runs: McpRunTarget::Host,
		description: "Browser automation via Playwright: navigate, click, type, take accessibility snapshots and \
		              screenshots of real pages. Use it to exercise or debug a running web app."
			.into(),
	}]
}

/// Merge presets + a workspace's custom servers into the settings
/// UI's row shape. Presets first, in registry order; customs after,
/// in insertion order. A custom entry whose id collides with a
/// preset is skipped (the preset wins).
pub fn server_rows(config: &CoderMcpWorkspaceConfig) -> Vec<McpServerStatus> {
	let presets = preset_servers();
	let mut rows: Vec<McpServerStatus> = presets
		.iter()
		.map(|preset| McpServerStatus {
			config: preset.clone(),
			preset: true,
			enabled: config.enabled.iter().any(|id| id == &preset.id),
		})
		.collect();
	for custom in &config.custom {
		if presets.iter().any(|preset| preset.id == custom.id) {
			continue;
		}
		rows.push(McpServerStatus {
			config: custom.clone(),
			preset: false,
			enabled: config.enabled.iter().any(|id| id == &custom.id),
		});
	}
	rows
}

/// The subset of servers currently enabled for a workspace, in row
/// order. This is what the meta-tool definitions advertise and what
/// dispatch validates against.
pub fn enabled_servers(config: &CoderMcpWorkspaceConfig) -> Vec<McpServerConfig> {
	server_rows(config)
		.into_iter()
		.filter(|row| row.enabled)
		.map(|row| row.config)
		.collect()
}

/// Owns the live connections, keyed by server id. Connections are
/// spawned lazily on first use and kept alive across turns — that's
/// deliberate: playwright's value is a browser session that persists
/// between `mcp_call`s. Children die with the IDE (`kill_on_drop`)
/// or when the user disables the server.
#[derive(Default)]
pub struct McpManager {
	connections: Mutex<HashMap<String, Arc<McpConnection>>>,
}

impl McpManager {
	/// `tools/list`, spawning + handshaking the server first if it
	/// isn't running yet. Follows `nextCursor` pagination.
	pub async fn list_tools(
		&self,
		config: &McpServerConfig,
		spawn: &McpSpawnTarget,
		cancel: &CancellationToken,
	) -> Result<Vec<Value>, CoderError> {
		let conn = self.connection(config, spawn, cancel).await?;
		let mut tools = Vec::new();
		let mut cursor: Option<String> = None;
		loop {
			let params = match &cursor {
				Some(c) => json!({ "cursor": c }),
				None => json!({}),
			};
			let result = self
				.request(&conn, config, "tools/list", params, HANDSHAKE_TIMEOUT, cancel)
				.await?;
			if let Some(page) = result.get("tools").and_then(Value::as_array) {
				tools.extend(page.iter().cloned());
			}
			match result.get("nextCursor").and_then(Value::as_str) {
				Some(next) if !next.is_empty() => cursor = Some(next.to_string()),
				_ => break,
			}
		}
		Ok(tools)
	}

	/// `tools/call`. An MCP-level `isError: true` result becomes a
	/// thrown [`CoderError`] carrying the server's content — the
	/// tool-error convention the loop already feeds back to the
	/// model as `isError: true`.
	pub async fn call_tool(
		&self,
		config: &McpServerConfig,
		spawn: &McpSpawnTarget,
		tool: &str,
		args: Value,
		cancel: &CancellationToken,
	) -> Result<Value, CoderError> {
		let conn = self.connection(config, spawn, cancel).await?;
		let params = json!({ "name": tool, "arguments": args });
		let result = self
			.request(&conn, config, "tools/call", params, CALL_TIMEOUT, cancel)
			.await?;
		let text = render_content(&result);
		if result.get("isError").and_then(Value::as_bool).unwrap_or(false) {
			return Err(CoderError::tool_failed("mcp_call", text));
		}
		Ok(json!({
			"server": config.id,
			"tool": tool,
			"content": text,
		}))
	}

	/// Drop (and thereby kill) a server's connection, if any. Called
	/// when the user disables or removes the server; also the
	/// recovery path after a request-level failure so the next call
	/// respawns fresh.
	pub async fn drop_connection(&self, id: &str) {
		self.connections.lock().await.remove(id);
	}

	async fn connection(
		&self,
		config: &McpServerConfig,
		spawn: &McpSpawnTarget,
		cancel: &CancellationToken,
	) -> Result<Arc<McpConnection>, CoderError> {
		let mut connections = self.connections.lock().await;
		if let Some(existing) = connections.get(&config.id) {
			if existing.alive() {
				return Ok(existing.clone());
			}
			connections.remove(&config.id);
		}
		let conn = Arc::new(McpConnection::spawn(config, spawn)?);
		// Handshake while holding the map lock: serialises
		// concurrent first-calls onto one spawn instead of racing
		// two children for the same server id.
		let init_params = json!({
			"protocolVersion": MCP_PROTOCOL_VERSION,
			"capabilities": {},
			"clientInfo": { "name": "moon-ide", "version": env!("CARGO_PKG_VERSION") },
		});
		conn
			.request("initialize", init_params, HANDSHAKE_TIMEOUT, cancel)
			.await
			.map_err(|err| {
				CoderError::tool_failed(
					"mcp_call",
					format!("MCP server `{}` failed to initialize: {err}", config.id),
				)
			})?;
		conn.notify("notifications/initialized", json!({})).await?;
		connections.insert(config.id.clone(), conn.clone());
		Ok(conn)
	}

	async fn request(
		&self,
		conn: &Arc<McpConnection>,
		config: &McpServerConfig,
		method: &str,
		params: Value,
		timeout: Duration,
		cancel: &CancellationToken,
	) -> Result<Value, CoderError> {
		let result = conn.request(method, params, timeout, cancel).await;
		if let Err(err) = &result {
			// A dead child can't serve the next call either — drop
			// the connection so it respawns. Aborts keep the
			// connection: the server is fine, the user just hit Esc.
			if !matches!(err, CoderError::Aborted) && !conn.alive() {
				self.drop_connection(&config.id).await;
			}
		}
		result
	}
}

/// Where to spawn a server process, resolved by the caller (the
/// tool registry knows the workspace's container name + cwd; this
/// module doesn't probe docker itself).
pub enum McpSpawnTarget {
	Host { cwd: String },
	Container { name: String, cwd: String },
}

/// In-flight requests parked on their response oneshots, keyed by
/// JSON-RPC id. `Err` carries the server's error message (or
/// "exited").
type PendingMap = Arc<std::sync::Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

struct McpConnection {
	child: std::sync::Mutex<Child>,
	stdin: Mutex<ChildStdin>,
	pending: PendingMap,
	next_id: AtomicU64,
}

impl McpConnection {
	fn spawn(config: &McpServerConfig, target: &McpSpawnTarget) -> Result<Self, CoderError> {
		let mut command = match target {
			McpSpawnTarget::Host { cwd } => {
				let mut command = tokio::process::Command::new(&config.command);
				command.args(&config.args).current_dir(cwd);
				command
			}
			// `-i` keeps stdin open — that *is* the transport.
			// No `-t`: a TTY would garble the JSON framing.
			McpSpawnTarget::Container { name, cwd } => {
				let mut command = tokio::process::Command::new("docker");
				command
					.arg("exec")
					.arg("-i")
					.arg("-w")
					.arg(cwd)
					.arg(name)
					.arg(&config.command)
					.args(&config.args);
				command
			}
		};
		command
			.stdin(std::process::Stdio::piped())
			.stdout(std::process::Stdio::piped())
			.stderr(std::process::Stdio::piped())
			.kill_on_drop(true);
		let mut child = command.spawn().map_err(|err| {
			CoderError::tool_failed(
				"mcp_call",
				format!("could not spawn MCP server `{}` ({}): {err}", config.id, config.command),
			)
		})?;
		let stdout = child
			.stdout
			.take()
			.ok_or_else(|| CoderError::Internal("mcp: child stdout not piped".into()))?;
		let stdin = child
			.stdin
			.take()
			.ok_or_else(|| CoderError::Internal("mcp: child stdin not piped".into()))?;
		let pending: PendingMap = Arc::new(std::sync::Mutex::new(HashMap::new()));
		// Reader: one task per connection routes responses to the
		// parked oneshots. Requests *from* the server (we advertise
		// no capabilities, so none are expected) and notifications
		// are ignored. On EOF every pending call fails with "server
		// exited".
		let reader_pending = pending.clone();
		let server_id = config.id.clone();
		tokio::spawn(async move {
			let mut lines = BufReader::new(stdout).lines();
			while let Ok(Some(line)) = lines.next_line().await {
				let Ok(message) = serde_json::from_str::<Value>(&line) else {
					tracing::debug!(server = %server_id, "mcp: skipping non-JSON stdout line");
					continue;
				};
				let Some(id) = message.get("id").and_then(Value::as_u64) else {
					continue;
				};
				let Some(sender) = reader_pending.lock().expect("mcp pending lock").remove(&id) else {
					continue;
				};
				let outcome = if let Some(error) = message.get("error") {
					Err(
						error
							.get("message")
							.and_then(Value::as_str)
							.map(str::to_string)
							.unwrap_or_else(|| error.to_string()),
					)
				} else {
					Ok(message.get("result").cloned().unwrap_or(Value::Null))
				};
				let _ = sender.send(outcome);
			}
			let orphans: Vec<_> = reader_pending.lock().expect("mcp pending lock").drain().collect();
			for (_, sender) in orphans {
				let _ = sender.send(Err("MCP server exited".into()));
			}
		});
		// Stderr → debug logs. MCP servers commonly chat on stderr
		// (npx progress bars, playwright banners); useful when a
		// server misbehaves, noise otherwise.
		if let Some(stderr) = child.stderr.take() {
			let server_id = config.id.clone();
			tokio::spawn(async move {
				let mut lines = BufReader::new(stderr).lines();
				while let Ok(Some(line)) = lines.next_line().await {
					tracing::debug!(server = %server_id, "mcp stderr: {line}");
				}
			});
		}
		Ok(Self {
			child: std::sync::Mutex::new(child),
			stdin: Mutex::new(stdin),
			pending,
			next_id: AtomicU64::new(1),
		})
	}

	fn alive(&self) -> bool {
		matches!(self.child.lock().expect("mcp child lock").try_wait(), Ok(None))
	}

	async fn notify(&self, method: &str, params: Value) -> Result<(), CoderError> {
		let message = json!({ "jsonrpc": "2.0", "method": method, "params": params });
		self.write_line(&message).await
	}

	async fn request(
		&self,
		method: &str,
		params: Value,
		timeout: Duration,
		cancel: &CancellationToken,
	) -> Result<Value, CoderError> {
		let id = self.next_id.fetch_add(1, Ordering::Relaxed);
		let (tx, rx) = oneshot::channel();
		self.pending.lock().expect("mcp pending lock").insert(id, tx);
		let message = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
		if let Err(err) = self.write_line(&message).await {
			self.pending.lock().expect("mcp pending lock").remove(&id);
			return Err(err);
		}
		let outcome = tokio::select! {
			_ = cancel.cancelled() => {
				self.pending.lock().expect("mcp pending lock").remove(&id);
				return Err(CoderError::Aborted);
			}
			outcome = tokio::time::timeout(timeout, rx) => outcome,
		};
		match outcome {
			Ok(Ok(Ok(result))) => Ok(result),
			Ok(Ok(Err(message))) => Err(CoderError::tool_failed("mcp_call", message)),
			// Sender dropped without a reply — reader task ended.
			Ok(Err(_)) => Err(CoderError::tool_failed("mcp_call", "MCP server exited")),
			Err(_) => {
				self.pending.lock().expect("mcp pending lock").remove(&id);
				Err(CoderError::tool_failed(
					"mcp_call",
					format!("MCP request `{method}` timed out after {}s", timeout.as_secs()),
				))
			}
		}
	}

	async fn write_line(&self, message: &Value) -> Result<(), CoderError> {
		let mut line =
			serde_json::to_string(message).map_err(|err| CoderError::Internal(format!("mcp: serialize request: {err}")))?;
		line.push('\n');
		let mut stdin = self.stdin.lock().await;
		stdin
			.write_all(line.as_bytes())
			.await
			.map_err(|err| CoderError::tool_failed("mcp_call", format!("MCP server stdin closed: {err}")))?;
		stdin
			.flush()
			.await
			.map_err(|err| CoderError::tool_failed("mcp_call", format!("MCP server stdin closed: {err}")))
	}
}

/// Flatten a `tools/call` result's content blocks to the text the
/// model sees. Text blocks pass through; image / audio / resource
/// blocks become placeholders — feeding tool-result images back to
/// the model is future work (the pi JSONL tool-result shape is
/// text-only today).
fn render_content(result: &Value) -> String {
	let Some(blocks) = result.get("content").and_then(Value::as_array) else {
		return result.to_string();
	};
	let mut out = String::new();
	for block in blocks {
		if !out.is_empty() {
			out.push('\n');
		}
		match block.get("type").and_then(Value::as_str) {
			Some("text") => out.push_str(block.get("text").and_then(Value::as_str).unwrap_or_default()),
			Some("image") => {
				let mime = block.get("mimeType").and_then(Value::as_str).unwrap_or("image");
				let bytes = block.get("data").and_then(Value::as_str).map(str::len).unwrap_or(0);
				out.push_str(&format!("[{mime} image, ~{} kB base64 — not displayed]", bytes / 1000));
			}
			Some("resource") | Some("resource_link") => {
				out.push_str(&format!(
					"[resource: {}]",
					block.get("resource").unwrap_or(&Value::Null)
				));
			}
			_ => out.push_str(&block.to_string()),
		}
	}
	out
}

/// Mint an id for a custom server — `mcp-<unix-ms>`, same shape
/// as [`crate::providers::new_provider_id`] minus the entropy
/// suffix (custom servers are added one click at a time; two in
/// the same millisecond doesn't happen).
pub fn new_custom_id() -> String {
	let ms = std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.unwrap_or_default()
		.as_millis();
	format!("mcp-{ms}")
}

/// Validate a user-supplied custom server id/label/command. Kept
/// here so the Tauri command layer and any future config import
/// share one rule set.
pub fn validate_custom(config: &McpServerConfig) -> Result<(), CoderError> {
	if config.label.trim().is_empty() {
		return Err(CoderError::invalid_args("mcp", "label must not be empty"));
	}
	if config.command.trim().is_empty() {
		return Err(CoderError::invalid_args("mcp", "command must not be empty"));
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;

	fn custom(id: &str) -> McpServerConfig {
		McpServerConfig {
			id: id.into(),
			label: id.into(),
			command: "echo".into(),
			..Default::default()
		}
	}

	#[test]
	fn rows_merge_presets_and_customs_with_enabled_flags() {
		let config = CoderMcpWorkspaceConfig {
			enabled: vec!["playwright".into(), "mcp-1".into()],
			custom: vec![custom("mcp-1"), custom("mcp-2")],
		};
		let rows = server_rows(&config);
		assert_eq!(rows.len(), 3);
		assert!(rows[0].preset && rows[0].enabled);
		assert_eq!(rows[1].config.id, "mcp-1");
		assert!(!rows[1].preset && rows[1].enabled);
		assert!(!rows[2].enabled);
	}

	#[test]
	fn custom_id_colliding_with_preset_is_skipped() {
		let config = CoderMcpWorkspaceConfig {
			enabled: vec![],
			custom: vec![custom("playwright")],
		};
		let rows = server_rows(&config);
		assert_eq!(rows.len(), 1);
		assert!(rows[0].preset);
		assert_eq!(rows[0].config.command, "npx");
	}

	#[test]
	fn enabled_servers_filters_and_keeps_order() {
		let config = CoderMcpWorkspaceConfig {
			enabled: vec!["mcp-2".into()],
			custom: vec![custom("mcp-1"), custom("mcp-2")],
		};
		let enabled = enabled_servers(&config);
		assert_eq!(enabled.len(), 1);
		assert_eq!(enabled[0].id, "mcp-2");
	}

	/// End-to-end over a real child process: spawn a minimal
	/// stdio MCP server (inline Node script), handshake, list,
	/// call, and error-path. Skips silently when `node` isn't on
	/// PATH — the dev toolchain ships it, minimal CI might not.
	#[tokio::test]
	async fn client_handshakes_lists_and_calls_against_fake_server() {
		if !std::process::Command::new("node")
			.arg("--version")
			.output()
			.map(|o| o.status.success())
			.unwrap_or(false)
		{
			eprintln!("skipping: node not on PATH");
			return;
		}
		const FAKE_SERVER: &str = r#"
const rl = require('readline').createInterface({ input: process.stdin });
rl.on('line', (line) => {
	const msg = JSON.parse(line);
	const reply = (result) => process.stdout.write(JSON.stringify({ jsonrpc: '2.0', id: msg.id, result }) + '\n');
	if (msg.method === 'initialize') {
		reply({ protocolVersion: '2025-06-18', capabilities: { tools: {} }, serverInfo: { name: 'fake', version: '1.0' } });
	} else if (msg.method === 'tools/list') {
		reply({ tools: [{ name: 'echo', description: 'echoes', inputSchema: { type: 'object' } }] });
	} else if (msg.method === 'tools/call' && msg.params.name === 'echo') {
		reply({ content: [{ type: 'text', text: 'echo: ' + msg.params.arguments.text }], isError: false });
	} else if (msg.method === 'tools/call') {
		reply({ content: [{ type: 'text', text: 'no such tool' }], isError: true });
	}
});
"#;
		let config = McpServerConfig {
			id: "fake".into(),
			label: "Fake".into(),
			command: "node".into(),
			args: vec!["-e".into(), FAKE_SERVER.into()],
			..Default::default()
		};
		let spawn = McpSpawnTarget::Host { cwd: "/tmp".into() };
		let manager = McpManager::default();
		let cancel = CancellationToken::new();

		let tools = manager.list_tools(&config, &spawn, &cancel).await.expect("tools/list");
		assert_eq!(tools.len(), 1);
		assert_eq!(tools[0].get("name").and_then(Value::as_str), Some("echo"));

		let result = manager
			.call_tool(&config, &spawn, "echo", json!({ "text": "hi" }), &cancel)
			.await
			.expect("tools/call");
		assert_eq!(result.get("content").and_then(Value::as_str), Some("echo: hi"));

		let err = manager
			.call_tool(&config, &spawn, "nope", json!({}), &cancel)
			.await
			.expect_err("isError result throws");
		assert!(err.to_string().contains("no such tool"), "got: {err}");

		manager.drop_connection("fake").await;
	}

	#[test]
	fn render_content_flattens_blocks() {
		let result = json!({
			"content": [
				{ "type": "text", "text": "hello" },
				{ "type": "image", "mimeType": "image/png", "data": "AAAA" },
			]
		});
		let text = render_content(&result);
		assert!(text.starts_with("hello\n["));
		assert!(text.contains("image/png"));
	}
}
