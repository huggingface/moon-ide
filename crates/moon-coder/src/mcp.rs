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

use moon_protocol::coder_mcp::{CoderMcpWorkspaceConfig, McpServerConfig, McpServerStatus};
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

/// The `@playwright/mcp` version the preset spawns. Pinned, not
/// `@latest`: the server only launches the chromium revision its own
/// playwright dependency expects, and moon-base pre-installs exactly
/// that revision (`images/moon-base/Dockerfile` derives it from this
/// same pin — keep the `PLAYWRIGHT_MCP_VERSION` ARG there in sync).
/// `@latest` drifted a revision ahead within days of an image bake
/// and every fresh container regressed to a 170 MB download or a
/// hard "not installed" loop (ADR 0033).
pub const PLAYWRIGHT_MCP_VERSION: &str = "0.0.79";

/// The curated preset list. Bootstrap posture: playwright is the
/// server the team actually asked for; further presets are added
/// when someone needs them, not preemptively.
pub fn preset_servers() -> Vec<McpServerConfig> {
	vec![McpServerConfig {
		id: "playwright".into(),
		label: "Playwright".into(),
		command: "npx".into(),
		// `--browser chromium` pins Playwright's bundled Chromium
		// (`npx playwright install chromium`) instead of the
		// server's default `chrome` channel, which requires real
		// Google Chrome at /opt/google/chrome — not installable on
		// every distro and not something a dev box should need.
		args: vec![
			"-y".into(),
			format!("@playwright/mcp@{PLAYWRIGHT_MCP_VERSION}"),
			"--browser".into(),
			"chromium".into(),
			// The coder drives the browser via snapshots; a headed
			// window popping open on the dev's display is noise,
			// and the server runs in the (display-less) workspace
			// container.
			"--headless".into(),
		],
		env: Default::default(),
		// No `--output-dir`: artefacts land in the server's own
		// default, `<roots[0]>/.playwright-mcp`, which the `roots`
		// capability makes deterministic (see ADR 0033). The
		// trade-off is a visible untracked dir in repos that don't
		// ignore it.
		//
		// The screenshot guidance below exists because a session
		// burned four tool calls hunting for a `filename` shot:
		// playwright resolves a bare `filename` against the
		// workspace root (not `.playwright-mcp/`), reports it as
		// an anchorless `./name.png`, and only returns pixels
		// inline when *it* names the file. Omitting `filename` is
		// strictly better for the coder — no path to guess, no
		// stray file in the repo.
		description: "Browser automation via Playwright: navigate, click, type, take accessibility snapshots and \
		              screenshots of real pages. Use it to exercise or debug a running web app. For screenshots, \
		              omit `filename` — the image comes back inline; a supplied `filename` saves into the \
		              workspace root (leaving a file you must clean up) and returns no pixels."
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
	/// `images_ok` gates the typed-image path: `false` (active model
	/// takes no image input) renders image blocks as "not attached"
	/// notes steering the model toward text alternatives, and skips
	/// the attachment collection entirely.
	pub async fn call_tool(
		&self,
		config: &McpServerConfig,
		spawn: &McpSpawnTarget,
		tool: &str,
		args: Value,
		cancel: &CancellationToken,
		images_ok: bool,
	) -> Result<Value, CoderError> {
		let conn = self.connection(config, spawn, cancel).await?;
		let params = json!({ "name": tool, "arguments": args });
		let result = self
			.request(&conn, config, "tools/call", params, CALL_TIMEOUT, cancel)
			.await?;
		let text = spawn.host_paths(&render_content(&result, images_ok));
		if result.get("isError").and_then(Value::as_bool).unwrap_or(false) {
			return Err(CoderError::tool_failed("mcp_call", text));
		}
		let images = if images_ok {
			collect_images(&result).await
		} else {
			Vec::new()
		};
		let mut out = json!({
			"server": config.id,
			"tool": tool,
			"content": text,
		});
		// `images` is the runner's typed-image convention: the
		// key leaves the text projection and reaches the model as
		// real image blocks. A playwright screenshot is the case
		// this exists for.
		if !images.is_empty() {
			out
				.as_object_mut()
				.expect("mcp result object")
				.insert("images".into(), serde_json::to_value(images).unwrap_or_default());
		}
		Ok(out)
	}

	/// Drop (and thereby kill) a server's connection, if any. Called
	/// when the user disables or removes the server; also the
	/// recovery path after a request-level failure so the next call
	/// respawns fresh.
	pub async fn drop_connection(&self, id: &str) {
		self.connections.lock().await.remove(id);
	}

	/// Drop every live connection. Called when the session's bash
	/// target flips (host ↔ container, ADR 0041): a cached
	/// connection would otherwise keep serving calls from the
	/// *previous* target's environment — a playwright browser in a
	/// container the session no longer runs in. The next call
	/// respawns on the fresh target.
	pub async fn drop_all_connections(&self) {
		self.connections.lock().await.clear();
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
		// `roots` tells the server which directories this session
		// is about — the workspace's bound folders. Servers use it
		// to scope file access and to resolve relative paths
		// (playwright derives both its output dir and its
		// file-access sandbox from the first root), so declaring
		// it beats every per-server path workaround. `listChanged:
		// false` because we don't push updates: a server picks its
		// roots up at handshake, and a bound-folder change during
		// a live session is rare enough to be covered by the
		// respawn on disable/enable.
		let init_params = json!({
			"protocolVersion": MCP_PROTOCOL_VERSION,
			"capabilities": { "roots": { "listChanged": false } },
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

/// One workspace directory advertised to servers over the MCP
/// `roots` capability, in the *server's* path space (container
/// paths for a container spawn).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpRoot {
	pub path: String,
	pub name: String,
}

/// A bound folder's bind-mount pair, used to rewrite paths a
/// container-side server reports back into host paths the coder's
/// filesystem tools resolve against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpMount {
	pub host_root: String,
	pub container_root: String,
}

/// Where to spawn a server process, resolved by the caller (the
/// tool registry knows the workspace's container name + cwd; this
/// module doesn't probe docker itself). MCP servers follow the
/// workspace: container when the shell is running, host
/// otherwise — wherever the coder's `bash` would run. There is
/// deliberately no per-server knob (an early `runs` config field
/// was removed unused; the workspace mode *is* the answer).
pub struct McpSpawnTarget {
	pub kind: McpSpawnKind,
	/// Bound folders to advertise via `roots/list`, **active
	/// folder first** — servers that accept a single workspace
	/// root take the first one (playwright derives its output
	/// dir and its file-access sandbox from it).
	pub roots: Vec<McpRoot>,
	/// Mount pairs for container→host path rewriting. Empty for
	/// a host spawn (paths already are host paths).
	pub mounts: Vec<McpMount>,
}

pub enum McpSpawnKind {
	Host { cwd: String },
	Container { name: String, cwd: String },
}

impl McpSpawnTarget {
	/// Rewrite container-local absolute paths in `text` to their
	/// host-side equivalent, so the model can hand a reported
	/// file path straight to the (host-side) filesystem tools.
	/// Covers every bound folder's mount, not just the active
	/// one — a server told about all roots can report a path in
	/// any of them. No-op for a host spawn.
	fn host_paths(&self, text: &str) -> String {
		let mut out = text.to_owned();
		for mount in &self.mounts {
			out = replace_path_prefix(&out, &mount.container_root, &mount.host_root);
		}
		out
	}
}

/// Replace every occurrence of `from` in `text` with `to`, but only
/// where `from` ends at a **path boundary** — end of string, or a
/// character that can't continue a path segment. Without the
/// boundary check, mapping `/workspace/app` would also rewrite the
/// unrelated `/workspace/app-2`. Servers embed paths in prose
/// (`saved to <p>`, `denied: <p>, <p>`), so the boundary set has to
/// cover punctuation, not just `/`.
fn replace_path_prefix(text: &str, from: &str, to: &str) -> String {
	if from.is_empty() {
		return text.to_owned();
	}
	let mut out = String::with_capacity(text.len());
	let mut rest = text;
	while let Some(idx) = rest.find(from) {
		let (before, matched) = rest.split_at(idx);
		let after = &matched[from.len()..];
		out.push_str(before);
		let continues_segment = after
			.chars()
			.next()
			.is_some_and(|c| c.is_alphanumeric() || matches!(c, '.' | '_' | '-' | '~'));
		out.push_str(if continues_segment { from } else { to });
		rest = after;
	}
	out.push_str(rest);
	out
}

/// The `roots/list` result: one entry per bound folder as a
/// `file://` URI, in the server's own path space. Order is
/// meaningful — servers that only take a single workspace root use
/// the first, which is why the caller puts the active folder there.
fn roots_result(roots: &[McpRoot]) -> Value {
	let entries: Vec<Value> = roots
		.iter()
		.map(|root| {
			json!({
				"uri": file_uri(&root.path),
				"name": root.name,
			})
		})
		.collect();
	json!({ "roots": entries })
}

/// `file://` URI for an absolute path. Percent-encodes the few
/// characters that would otherwise break URI parsing on the
/// server side; the segment separator is left intact.
fn file_uri(path: &str) -> String {
	let mut out = String::from("file://");
	for ch in path.chars() {
		match ch {
			'%' => out.push_str("%25"),
			' ' => out.push_str("%20"),
			'#' => out.push_str("%23"),
			'?' => out.push_str("%3F"),
			c => out.push(c),
		}
	}
	out
}

/// Write one newline-delimited JSON-RPC message. Shared by the
/// request path and the reader task's reply path so the framing
/// lives in one place.
async fn write_line(stdin: &mut ChildStdin, message: &Value) -> Result<(), CoderError> {
	let mut line =
		serde_json::to_string(message).map_err(|err| CoderError::Internal(format!("mcp: serialize message: {err}")))?;
	line.push('\n');
	stdin
		.write_all(line.as_bytes())
		.await
		.map_err(|err| CoderError::tool_failed("mcp_call", format!("MCP server stdin closed: {err}")))?;
	stdin
		.flush()
		.await
		.map_err(|err| CoderError::tool_failed("mcp_call", format!("MCP server stdin closed: {err}")))
}

/// In-flight requests parked on their response oneshots, keyed by
/// JSON-RPC id. `Err` carries the server's error message (or
/// "exited").
type PendingMap = Arc<std::sync::Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

struct McpConnection {
	child: std::sync::Mutex<Child>,
	/// Shared with the reader task, which writes responses to
	/// server→client requests (`roots/list`) on the same pipe.
	stdin: Arc<Mutex<ChildStdin>>,
	pending: PendingMap,
	next_id: AtomicU64,
}

impl McpConnection {
	fn spawn(config: &McpServerConfig, target: &McpSpawnTarget) -> Result<Self, CoderError> {
		let mut command = match &target.kind {
			McpSpawnKind::Host { cwd } => {
				let mut command = tokio::process::Command::new(&config.command);
				command.args(&config.args).current_dir(cwd).envs(&config.env);
				command
			}
			// `-i` keeps stdin open — that *is* the transport.
			// No `-t`: a TTY would garble the JSON framing.
			McpSpawnKind::Container { name, cwd } => {
				let mut command = tokio::process::Command::new("docker");
				command.arg("exec").arg("-i").arg("-w").arg(cwd);
				// `docker exec` inherits the container's image env only
				// (no login shell, no .bashrc), so per-server env has to
				// ride on the exec call itself.
				for (key, value) in &config.env {
					command.arg("-e").arg(format!("{key}={value}"));
				}
				command.arg(name).arg(&config.command).args(&config.args);
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
		let stdin = Arc::new(Mutex::new(
			child
				.stdin
				.take()
				.ok_or_else(|| CoderError::Internal("mcp: child stdin not piped".into()))?,
		));
		let pending: PendingMap = Arc::new(std::sync::Mutex::new(HashMap::new()));
		// Reader: one task per connection. Three kinds of inbound
		// message:
		//   - responses (`id`, no `method`) → the parked oneshot;
		//   - requests (`id` + `method`) → answered here, on the
		//     same pipe (that's `roots/list`, see `roots_result`);
		//   - notifications (`method`, no `id`) → logged, dropped.
		// On EOF every pending call fails with "server exited".
		let reader_pending = pending.clone();
		let reader_stdin = stdin.clone();
		let server_id = config.id.clone();
		let roots = target.roots.clone();
		tokio::spawn(async move {
			let mut lines = BufReader::new(stdout).lines();
			while let Ok(Some(line)) = lines.next_line().await {
				let Ok(message) = serde_json::from_str::<Value>(&line) else {
					tracing::debug!(server = %server_id, "mcp: skipping non-JSON stdout line");
					continue;
				};
				let method = message.get("method").and_then(Value::as_str);
				let Some(id) = message.get("id").and_then(Value::as_u64) else {
					if let Some(method) = method {
						tracing::debug!(server = %server_id, method, "mcp: ignoring server notification");
					}
					continue;
				};
				// Server→client request. Every request must get a
				// reply or the server blocks forever waiting —
				// hence the explicit "method not found" for
				// anything we don't implement.
				if let Some(method) = method {
					let reply = match method {
						"roots/list" => json!({ "jsonrpc": "2.0", "id": id, "result": roots_result(&roots) }),
						_ => json!({
							"jsonrpc": "2.0",
							"id": id,
							"error": { "code": -32601, "message": format!("method `{method}` not supported by moon-ide") },
						}),
					};
					let mut guard = reader_stdin.lock().await;
					if let Err(err) = write_line(&mut guard, &reply).await {
						tracing::debug!(server = %server_id, error = %err, method, "mcp: failed to answer server request");
					}
					continue;
				}
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
			stdin,
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
		let mut stdin = self.stdin.lock().await;
		write_line(&mut stdin, message).await
	}
}

/// Largest image we forward to the model from an MCP result,
/// measured in base64 characters (~1.5 MB decoded). Playwright
/// screenshots at default viewport are far below this; anything
/// bigger stays a placeholder so one result can't crowd out the
/// context window.
const IMAGE_MAX_BASE64_CHARS: usize = 2_000_000;

/// Flatten a `tools/call` result's content blocks to the text the
/// model reads. Text blocks pass through; image blocks become a
/// one-line placeholder — the pixels themselves ride separately
/// (see [`collect_images`]) when `images_ok`, or not at all when
/// the active model takes no image input (the placeholder then
/// says so and points at text alternatives, e.g. playwright's
/// accessibility snapshot instead of a screenshot); audio /
/// resource blocks stay placeholders.
fn render_content(result: &Value, images_ok: bool) -> String {
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
				if images_ok {
					out.push_str(&format!("[{mime} image attached — ~{} kB]", bytes / 1000));
				} else {
					out.push_str(&format!(
						"[{mime} image not attached — the active model does not accept image input; use a text \
						 alternative (e.g. an accessibility snapshot instead of a screenshot)]"
					));
				}
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

/// Collect a `tools/call` result's image blocks as typed
/// attachments, for the runner's `images` convention. Oversized
/// or malformed blocks are skipped (their placeholder stays in
/// the rendered text either way).
///
/// Async because screenshots get re-encoded on the way through
/// (see [`crate::images`]) and that's CPU-bound enough to belong
/// on the blocking pool — a playwright frame runs ~0.2 s.
async fn collect_images(result: &Value) -> Vec<crate::inference::ImageAttachment> {
	let Some(blocks) = result.get("content").and_then(Value::as_array) else {
		return Vec::new();
	};
	let mut images = Vec::new();
	for block in blocks {
		if block.get("type").and_then(Value::as_str) != Some("image") {
			continue;
		}
		let Some(data) = block.get("data").and_then(Value::as_str) else {
			continue;
		};
		if data.is_empty() || data.len() > IMAGE_MAX_BASE64_CHARS {
			tracing::debug!(bytes = data.len(), "mcp: skipping oversized or empty image block");
			continue;
		}
		let data = data.to_string();
		let mime = block
			.get("mimeType")
			.and_then(Value::as_str)
			.unwrap_or("image/png")
			.to_string();
		match tokio::task::spawn_blocking(move || crate::images::attachment_from_base64(&data, &mime)).await {
			Ok(attachment) => images.push(attachment),
			Err(err) => tracing::warn!(error = %err, "mcp: image encoding task died; dropping the block"),
		}
	}
	images
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

	/// The moon-base image pre-installs the chromium revision the
	/// preset's pinned `@playwright/mcp` expects; a drifted pin
	/// regresses every fresh container to a 170 MB download or a
	/// "not installed" loop. Parse the Dockerfile's ARG and hold
	/// the two in lockstep.
	#[test]
	fn playwright_pin_matches_moon_base_dockerfile() {
		let dockerfile = concat!(env!("CARGO_MANIFEST_DIR"), "/../../images/moon-base/Dockerfile");
		let contents = std::fs::read_to_string(dockerfile).expect("moon-base Dockerfile readable");
		let arg = contents
			.lines()
			.find_map(|line| line.trim().strip_prefix("ARG PLAYWRIGHT_MCP_VERSION="))
			.expect("Dockerfile declares ARG PLAYWRIGHT_MCP_VERSION");
		assert_eq!(
			arg, PLAYWRIGHT_MCP_VERSION,
			"images/moon-base/Dockerfile pins @playwright/mcp@{arg} but the preset spawns \
			 @playwright/mcp@{PLAYWRIGHT_MCP_VERSION} — keep them in sync (see ADR 0033)"
		);
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
		// The fake server exercises the inbound direction too: on
		// `initialize` it fires a `roots/list` request back at us
		// (like playwright does) and then reports what it got as
		// an `echo_roots` tool result.
		const FAKE_SERVER: &str = r#"
const rl = require('readline').createInterface({ input: process.stdin });
let seenRoots = 'none';
let nextId = 1000;
rl.on('line', (line) => {
	const msg = JSON.parse(line);
	const reply = (result) => process.stdout.write(JSON.stringify({ jsonrpc: '2.0', id: msg.id, result }) + '\n');
	if (msg.method === 'initialize') {
		const caps = msg.params && msg.params.capabilities || {};
		reply({ protocolVersion: '2025-06-18', capabilities: { tools: {} }, serverInfo: { name: 'fake', version: '1.0' } });
		if (caps.roots) {
			process.stdout.write(JSON.stringify({ jsonrpc: '2.0', id: nextId++, method: 'roots/list' }) + '\n');
			// Also probe an unsupported request: we must get an
			// error reply rather than silence.
			process.stdout.write(JSON.stringify({ jsonrpc: '2.0', id: nextId++, method: 'sampling/createMessage' }) + '\n');
		}
		return;
	}
	if (msg.method === undefined && msg.result && msg.result.roots) {
		seenRoots = msg.result.roots.map((r) => r.uri).join(',');
		return;
	}
	if (msg.method === undefined && msg.error) {
		seenRoots += '|err:' + msg.error.code;
		return;
	}
	if (msg.method === 'tools/list') {
		reply({ tools: [{ name: 'echo', description: 'echoes', inputSchema: { type: 'object' } }] });
	} else if (msg.method === 'tools/call' && msg.params.name === 'echo_roots') {
		reply({ content: [{ type: 'text', text: seenRoots }], isError: false });
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
		let spawn = McpSpawnTarget {
			kind: McpSpawnKind::Host { cwd: "/tmp".into() },
			roots: vec![
				McpRoot {
					path: "/tmp/app".into(),
					name: "app".into(),
				},
				McpRoot {
					path: "/tmp/lib".into(),
					name: "lib".into(),
				},
			],
			mounts: Vec::new(),
		};
		let manager = McpManager::default();
		let cancel = CancellationToken::new();

		let tools = manager.list_tools(&config, &spawn, &cancel).await.expect("tools/list");
		assert_eq!(tools.len(), 1);
		assert_eq!(tools[0].get("name").and_then(Value::as_str), Some("echo"));

		let result = manager
			.call_tool(&config, &spawn, "echo", json!({ "text": "hi" }), &cancel, true)
			.await
			.expect("tools/call");
		assert_eq!(result.get("content").and_then(Value::as_str), Some("echo: hi"));

		// The server's `roots/list` was answered with both bound
		// folders, in order, and its unsupported request got a
		// JSON-RPC "method not found" instead of hanging.
		let roots = manager
			.call_tool(&config, &spawn, "echo_roots", json!({}), &cancel, true)
			.await
			.expect("tools/call echo_roots");
		assert_eq!(
			roots.get("content").and_then(Value::as_str),
			Some("file:///tmp/app,file:///tmp/lib|err:-32601")
		);

		let err = manager
			.call_tool(&config, &spawn, "nope", json!({}), &cancel, true)
			.await
			.expect_err("isError result throws");
		assert!(err.to_string().contains("no such tool"), "got: {err}");

		manager.drop_connection("fake").await;
	}

	/// A live connection answers calls without respawning
	/// (playwright's browser session persists between calls), and
	/// `drop_all_connections` — the host↔container toggle hook
	/// (ADR 0041) — clears it so the next call respawns fresh.
	#[tokio::test]
	async fn cached_connection_reused_until_drop_all() {
		if !std::process::Command::new("node")
			.arg("--version")
			.output()
			.map(|o| o.status.success())
			.unwrap_or(false)
		{
			eprintln!("skipping: node not on PATH");
			return;
		}
		// The fake server reports its pid as an `echo_pid` tool
		// result, so connection reuse vs. respawn is observable
		// through calls — never by locking the manager's map
		// (list_tools holds that lock across the spawn).
		const PID_SERVER: &str = r#"
const rl = require('readline').createInterface({ input: process.stdin });
rl.on('line', (line) => {
	const msg = JSON.parse(line);
	const reply = (result) => process.stdout.write(JSON.stringify({ jsonrpc: '2.0', id: msg.id, result }) + '\n');
	if (msg.method === 'initialize') {
		reply({ protocolVersion: '2025-06-18', capabilities: { tools: {} }, serverInfo: { name: 'pid', version: '1.0' } });
	} else if (msg.method === 'tools/list') {
		reply({ tools: [{ name: 'echo_pid', description: 'reports pid', inputSchema: { type: 'object' } }] });
	} else if (msg.method === 'tools/call' && msg.params.name === 'echo_pid') {
		reply({ content: [{ type: 'text', text: String(process.pid) }], isError: false });
	}
});
"#;
		let config = McpServerConfig {
			id: "pid".into(),
			label: "Pid".into(),
			command: "node".into(),
			args: vec!["-e".into(), PID_SERVER.into()],
			..Default::default()
		};
		let spawn = McpSpawnTarget {
			kind: McpSpawnKind::Host { cwd: "/tmp".into() },
			roots: Vec::new(),
			mounts: Vec::new(),
		};
		let manager = McpManager::default();
		let cancel = CancellationToken::new();
		let pid_of = async |manager: &McpManager| {
			manager
				.call_tool(&config, &spawn, "echo_pid", json!({}), &cancel, true)
				.await
				.expect("echo_pid")
				.get("content")
				.and_then(Value::as_str)
				.map(str::to_owned)
				.expect("pid text")
		};

		let first = pid_of(&manager).await;
		let second = pid_of(&manager).await;
		assert_eq!(first, second, "live connection must be reused");

		manager.drop_all_connections().await;
		let third = pid_of(&manager).await;
		assert_ne!(first, third, "drop_all must force a respawn");
		manager.drop_all_connections().await;
	}

	#[test]
	fn render_content_flattens_blocks() {
		let result = json!({
			"content": [
				{ "type": "text", "text": "hello" },
				{ "type": "image", "mimeType": "image/png", "data": "AAAA" },
			]
		});
		let text = render_content(&result, true);
		assert!(text.starts_with("hello\n["));
		assert!(text.contains("image/png"));
		assert!(text.contains("image attached"));

		// Same blocks for a text-only model: the placeholder flips
		// to a "not attached" note steering toward text sources.
		let text = render_content(&result, false);
		assert!(text.contains("image not attached"));
		assert!(text.contains("does not accept image input"));
	}

	#[tokio::test]
	async fn collect_images_builds_typed_attachments() {
		let result = json!({
			"content": [
				{ "type": "text", "text": "shot taken" },
				{ "type": "image", "mimeType": "image/png", "data": "QUJD" },
				{ "type": "image", "data": "" },
			]
		});
		let images = collect_images(&result).await;
		assert_eq!(images.len(), 1, "empty data blocks are skipped");
		assert_eq!(images[0].mime, "image/png");
		assert_eq!(images[0].data_url, "data:image/png;base64,QUJD");
	}

	#[tokio::test]
	async fn collect_images_skips_oversized_blocks() {
		let big = "A".repeat(IMAGE_MAX_BASE64_CHARS + 1);
		let result = json!({
			"content": [{ "type": "image", "mimeType": "image/png", "data": big }]
		});
		assert!(collect_images(&result).await.is_empty());
	}

	#[test]
	fn preset_runs_chromium_headless() {
		let preset = &preset_servers()[0];
		assert_eq!(preset.id, "playwright");
		assert!(preset.args.iter().any(|a| a == "--headless"));
		let chromium = preset
			.args
			.windows(2)
			.any(|w| w[0] == "--browser" && w[1] == "chromium");
		assert!(chromium, "preset should pin Playwright's bundled chromium");
	}

	/// A container spawn over two bound folders: `app` (active)
	/// and `lib`.
	fn container_target() -> McpSpawnTarget {
		McpSpawnTarget {
			kind: McpSpawnKind::Container {
				name: "moon-ws-default-dev-1".into(),
				cwd: "/workspace/app".into(),
			},
			roots: vec![
				McpRoot {
					path: "/workspace/app".into(),
					name: "app".into(),
				},
				McpRoot {
					path: "/workspace/lib".into(),
					name: "lib".into(),
				},
			],
			mounts: vec![
				McpMount {
					host_root: "/home/me/code/app".into(),
					container_root: "/workspace/app".into(),
				},
				McpMount {
					host_root: "/home/me/code/lib".into(),
					container_root: "/workspace/lib".into(),
				},
			],
		}
	}

	#[test]
	fn container_target_rewrites_reported_paths_to_host() {
		let target = container_target();
		let text = "### Screenshot\nsaved to /workspace/app/.playwright-mcp/page-1.png\nroot is /workspace/app";
		let rewritten = target.host_paths(text);
		assert_eq!(
			rewritten,
			"### Screenshot\nsaved to /home/me/code/app/.playwright-mcp/page-1.png\nroot is /home/me/code/app"
		);
	}

	/// Every advertised root is translated, not just the active
	/// folder's — a server told about all roots can report a path
	/// in any of them (the file-access-denied message lists them
	/// all, for instance).
	#[test]
	fn container_target_rewrites_sibling_folder_paths_too() {
		let target = container_target();
		let text = "denied: /workspace/lib/fixtures/a.png outside roots: /workspace/app, /workspace/lib";
		assert_eq!(
			target.host_paths(text),
			"denied: /home/me/code/lib/fixtures/a.png outside roots: /home/me/code/app, /home/me/code/lib"
		);
	}

	#[test]
	fn container_rewrite_leaves_unmounted_paths_alone() {
		let target = container_target();
		// Not a bound folder, and a same-prefix sibling that
		// must not be mangled into `/home/me/code/app-2`.
		let text = "see /workspace/other/file.txt and /workspace/app-2/x";
		assert_eq!(target.host_paths(text), text);
	}

	#[test]
	fn host_target_leaves_paths_untouched() {
		let target = McpSpawnTarget {
			kind: McpSpawnKind::Host {
				cwd: "/home/me/code/app".into(),
			},
			roots: vec![McpRoot {
				path: "/home/me/code/app".into(),
				name: "app".into(),
			}],
			mounts: Vec::new(),
		};
		let text = "saved to /home/me/code/app/.playwright-mcp/page-1.png";
		assert_eq!(target.host_paths(text), text);
	}

	#[test]
	fn replace_path_prefix_respects_segment_boundaries() {
		let f = |text| replace_path_prefix(text, "/ws/app", "/host/app");
		// Boundaries: separator, punctuation, quote, end of input.
		assert_eq!(f("/ws/app/x.png"), "/host/app/x.png");
		assert_eq!(f("a /ws/app, b"), "a /host/app, b");
		assert_eq!(f("(/ws/app)"), "(/host/app)");
		assert_eq!(f("\"/ws/app\""), "\"/host/app\"");
		assert_eq!(f("at /ws/app"), "at /host/app");
		assert_eq!(f("/ws/app:8080"), "/host/app:8080");
		// Not boundaries — a longer sibling directory name.
		assert_eq!(f("/ws/app-2/x"), "/ws/app-2/x");
		assert_eq!(f("/ws/apple"), "/ws/apple");
		assert_eq!(f("/ws/app.bak"), "/ws/app.bak");
		// Repeated occurrences all get rewritten.
		assert_eq!(f("/ws/app and /ws/app/x"), "/host/app and /host/app/x");
	}

	#[test]
	fn roots_result_emits_file_uris_in_order() {
		let target = container_target();
		let result = roots_result(&target.roots);
		let roots = result.get("roots").and_then(Value::as_array).expect("roots array");
		assert_eq!(roots.len(), 2);
		// Active folder first — servers that take a single
		// workspace root use `roots[0]`.
		assert_eq!(
			roots[0].get("uri").and_then(Value::as_str),
			Some("file:///workspace/app")
		);
		assert_eq!(roots[0].get("name").and_then(Value::as_str), Some("app"));
		assert_eq!(
			roots[1].get("uri").and_then(Value::as_str),
			Some("file:///workspace/lib")
		);
	}

	#[test]
	fn file_uri_escapes_characters_that_break_uri_parsing() {
		assert_eq!(file_uri("/home/me/my code"), "file:///home/me/my%20code");
		assert_eq!(file_uri("/tmp/a#b?c"), "file:///tmp/a%23b%3Fc");
		assert_eq!(file_uri("/tmp/100%"), "file:///tmp/100%25");
		// Separators stay intact.
		assert_eq!(file_uri("/a/b/c"), "file:///a/b/c");
	}
}
