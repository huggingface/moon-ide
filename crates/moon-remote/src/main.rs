//! moon-remote — headless moon-ide for remote machines.
//!
//! An enrolled IDE without a webview (see the crate doc in
//! `lib.rs`). Typical first-time setup on a remote box:
//!
//! ```text
//! moon-remote login                                  # HF device flow (opens hf.co/device on any browser)
//! moon-remote enroll --bridge wss://bridge.example --code XXXX-XXXX
//! moon-remote workspace-add --name myproject --folder ~/code/myproject
//! moon-remote serve --workspace myproject
//! ```
//!
//! The box needs a Secret Service (keyring) for the HF token,
//! provider API keys and the relay credential — on a headless
//! server run under `dbus-run-session` with an unlocked
//! `gnome-keyring-daemon`, same recipe as the standing relay
//! (ADR 0035).

use std::sync::Arc;

use camino::{Utf8Path, Utf8PathBuf};
use clap::{Parser, Subcommand};
use moon_app::settings::SettingsContext;
use moon_core::app_state as app_state_store;
use moon_core::session as core_session;
use moon_core::WorkspaceRegistry;
use moon_remote::relay;
use moon_remote::rpc::{BridgeRpc, WorkspaceLauncher};

/// Same dir identity as the desktop app (`BUNDLE_IDENTIFIER` in
/// `src-tauri`): the headless binary shares `state.json`,
/// per-workspace `session.json`, coder sessions and the keyring
/// with a desktop install on the same machine, so the two are
/// interchangeable hosts for the same workspaces.
const BUNDLE_IDENTIFIER: &str = "moon-ide";

#[derive(Parser, Debug)]
#[command(
	name = "moon-remote",
	version,
	about = "headless moon-ide: serve coder sessions to the companion via a moon-bridge relay"
)]
struct Args {
	#[command(subcommand)]
	command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
	/// Sign in to Hugging Face (device flow — prints a URL + code to
	/// approve from any browser). Stores the OAuth bundle in the OS
	/// keyring, same entry the desktop uses.
	Login,
	/// Enroll this machine with a moon-bridge relay. Run `moon-bridge
	/// serve` on the relay host and read the one-time code from its
	/// log; the resulting IDE token is stored in the keyring.
	Enroll {
		/// Relay URL, e.g. `wss://bridge.example.com`.
		#[arg(long)]
		bridge: String,
		/// One-time enrollment code from the relay's log.
		#[arg(long)]
		code: String,
		/// Label shown on the phone's switcher; defaults to the hostname.
		#[arg(long)]
		label: Option<String>,
	},
	/// Register a workspace: a catalog entry plus one bound folder.
	/// Equivalent to creating a workspace and opening a folder in the
	/// desktop UI.
	WorkspaceAdd {
		/// Human-readable workspace name (slug derived from it unless --slug).
		#[arg(long)]
		name: String,
		/// Absolute path of the project folder to bind.
		#[arg(long)]
		folder: Utf8PathBuf,
		/// Explicit slug (default: slugified name).
		#[arg(long)]
		slug: Option<String>,
	},
	/// Boot the coder for one workspace and serve it through the
	/// enrolled relay until interrupted.
	Serve {
		/// Workspace slug (see `workspace-add` / the desktop's picker).
		#[arg(long)]
		workspace: String,
	},
	/// Attach stdin/stdout to the workspace's `serve` process (ADR
	/// 0095), starting it in the background if it isn't running. The
	/// desktop runs this over `ssh` to open a remote workspace.
	Attach {
		#[arg(long)]
		workspace: String,
	},
	/// Manage the workspace shell container. When it's running,
	/// agents' `bash` / format / lint route into it exactly like
	/// the desktop (the routing is shared moon-coder code); when
	/// it isn't, everything runs host-side. Works against docker
	/// or a podman socket via `DOCKER_HOST`.
	Container {
		/// Workspace slug.
		#[arg(long)]
		workspace: String,
		/// Regenerate `compose.yaml` from the bound folders and
		/// `docker compose up -d --wait` (first run pulls/builds
		/// the dev image).
		#[arg(long)]
		up: bool,
		/// Stop and remove the compose project.
		#[arg(long)]
		stop: bool,
		/// Dev image for `--up`. Defaults to the desktop's default.
		#[arg(long)]
		image: Option<String>,
	},
	/// Unbind a project folder from a workspace (`session.json`).
	/// The inverse of `workspace-add`'s bind; sessions and the
	/// folder's files are untouched. **Stop the workspace's `serve`
	/// first** — a running instance rewrites `session.json` and
	/// would clobber this edit.
	WorkspaceRemoveFolder {
		/// Workspace slug.
		#[arg(long)]
		workspace: String,
		/// Absolute path of the bound folder to remove.
		#[arg(long)]
		folder: Utf8PathBuf,
	},
	/// List or toggle MCP servers for a workspace (`session.json`,
	/// same store the desktop settings panel writes). No flags =
	/// list. **Stop the workspace's `serve` before toggling** — a
	/// running instance rewrites `session.json` and would clobber
	/// this edit; it reads the config back at next boot.
	Mcp {
		/// Workspace slug.
		#[arg(long)]
		workspace: String,
		/// Server id to enable (e.g. `playwright`).
		#[arg(long)]
		enable: Option<String>,
		/// Server id to disable.
		#[arg(long)]
		disable: Option<String>,
	},
	/// Mint a phone pairing link for a running `serve` (ADR 0087):
	/// prints the link to copy-paste into the companion plus a
	/// terminal QR. The link carries this host's public key and a
	/// single-use secret; the relay only routes.
	Pair {
		/// Workspace slug whose `serve` should answer the pairing.
		#[arg(long)]
		workspace: String,
		/// Print the link only, no terminal QR.
		#[arg(long)]
		no_qr: bool,
	},
	/// List the phones allowed to drive this host end-to-end, or
	/// revoke one. Live sessions of a revoked phone end within 30 s.
	Devices {
		/// Device id to revoke (see the listing).
		#[arg(long)]
		revoke: Option<String>,
	},
	/// Show or set the model picks (`state.json`, same store the
	/// desktop picker writes). Prints the current picks when no flag
	/// is given. A running `serve` reads the picks at boot — restart
	/// its unit after changing them.
	Model {
		/// Standard model slug, e.g. `moonshotai/Kimi-K3` or
		/// `Qwen/Qwen3.5-397B-A17B:scaleway`. Empty string resets to
		/// the built-in default.
		#[arg(long)]
		standard: Option<String>,
		/// Cheap model slug (titles, summaries, commit messages).
		/// Empty string resets to the built-in default.
		#[arg(long)]
		cheap: Option<String>,
		/// Rate-limit fallback chain: comma-separated wire slugs
		/// tried in order when a request's 429 backoff is
		/// exhausted (e.g. `moonshotai/Kimi-K3:together,\
		/// deepseek-ai/DeepSeek-V4-Pro-0813:fireworks-ai`). Empty
		/// string clears the chain.
		#[arg(long)]
		rotation: Option<String>,
		/// Reasoning depth passed as `reasoning_effort` (e.g. medium,
		/// high, xhigh, max). Pass an empty string to clear it.
		#[arg(long)]
		reasoning: Option<String>,
	},
}

fn main() -> anyhow::Result<()> {
	// Install the ring crypto provider as the process default before
	// anything builds a rustls config — feature unification gives
	// this binary both `ring` (tokio-tungstenite) and `aws-lc-rs`
	// (via moon-coder's reqwest), and rustls refuses to auto-pick
	// between two providers: the relay client's `connect_async`
	// panics with "no process-level CryptoProvider" otherwise.
	// Same fix as the desktop (`src-tauri/src/lib.rs`).
	let _ = rustls::crypto::ring::default_provider().install_default();

	// stderr: `attach` uses stdout as its data channel.
	tracing_subscriber::fmt()
		.with_writer(std::io::stderr)
		.with_env_filter(
			tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,moon_remote=debug".into()),
		)
		.init();
	let args = Args::parse();
	let runtime = tokio::runtime::Runtime::new()?;
	runtime.block_on(async move {
		match args.command {
			Command::Login => login().await,
			Command::Enroll { bridge, code, label } => enroll(bridge, code, label).await,
			Command::WorkspaceAdd { name, folder, slug } => workspace_add(name, folder, slug).await,
			Command::WorkspaceRemoveFolder { workspace, folder } => workspace_remove_folder(workspace, folder).await,
			Command::Serve { workspace } => serve(workspace).await,
			Command::Attach { workspace } => attach(workspace).await,
			Command::Pair { workspace, no_qr } => pair(workspace, no_qr).await,
			Command::Devices { revoke } => devices(revoke),
			Command::Container {
				workspace,
				up,
				stop,
				image,
			} => container(workspace, up, stop, image).await,
			Command::Mcp {
				workspace,
				enable,
				disable,
			} => mcp(workspace, enable, disable).await,
			Command::Model {
				standard,
				cheap,
				rotation,
				reasoning,
			} => model(standard, cheap, rotation, reasoning).await,
		}
	})
}

/// `<XDG_CONFIG_HOME>/moon-ide` — `state.json` lives here.
fn config_dir() -> anyhow::Result<Utf8PathBuf> {
	let raw = dirs::config_dir().ok_or_else(|| anyhow::anyhow!("could not resolve config dir"))?;
	Utf8PathBuf::from_path_buf(raw.join(BUNDLE_IDENTIFIER))
		.map_err(|p| anyhow::anyhow!("non-utf8 config dir: {}", p.display()))
}

/// `<XDG_DATA_HOME>/moon-ide` and its `workspaces/` root (ADR 0007).
fn data_dirs() -> anyhow::Result<(Utf8PathBuf, Utf8PathBuf)> {
	let raw = dirs::data_local_dir().ok_or_else(|| anyhow::anyhow!("could not resolve local data dir"))?;
	let data = Utf8PathBuf::from_path_buf(raw.join(BUNDLE_IDENTIFIER))
		.map_err(|p| anyhow::anyhow!("non-utf8 data dir: {}", p.display()))?;
	let workspaces = data.join("workspaces");
	Ok((data, workspaces))
}

/// HF sign-in via the device flow. The `Authenticator` is the same
/// one the coder uses (keyring `moon-ide`/`hf-oauth`), so a desktop
/// install on this machine — or a `serve` right after — picks the
/// session up unchanged.
async fn login() -> anyhow::Result<()> {
	let auth = moon_coder::auth::Authenticator::new()?;
	if auth.has_valid_session().await {
		println!("Already signed in. (`moon-remote serve` will use the stored session.)");
		return Ok(());
	}
	let device = auth.start_device_flow().await?;
	match &device.verification_uri_complete {
		Some(url) => println!("Open   {url}"),
		None => println!("Open   {}", device.verification_uri),
	}
	println!("Code   {}", device.user_code);
	println!("Waiting for approval…");
	let identity = auth.poll_device_code(&device).await?;
	println!("Signed in as {}", identity.username);
	Ok(())
}

/// One-shot enrollment: dial the relay with the code, store the
/// credential, exit. `serve` picks the credential up afterwards.
async fn enroll(bridge: String, code: String, label: Option<String>) -> anyhow::Result<()> {
	// Reuse a previous ide_id when re-enrolling (e.g. after a relay
	// wipe) so the phone's switcher doesn't grow a second group.
	let ide_id = match relay::load_credential() {
		Ok(Some(cred)) => cred.ide_id,
		_ => relay::generate_ide_id(),
	};
	let label = label.unwrap_or_else(|| ide_id.clone());
	// No workspaces yet — `serve` re-registers with the real list on
	// every (re)connect.
	let rpc: Arc<dyn moon_remote::rpc::BridgeRpcHandler> = Arc::new(NullRpc);
	let handle = relay::spawn(bridge, code, ide_id.clone(), label, Vec::new(), rpc);
	let mut status_rx = handle.status_receiver();
	let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
	loop {
		let status = status_rx.borrow().clone();
		if status.connected {
			println!("Enrolled as `{ide_id}`. Run `moon-remote serve --workspace <slug>` to go live.");
			return Ok(());
		}
		if let Some(err) = status.error {
			anyhow::bail!("enrollment failed: {err}");
		}
		if tokio::time::Instant::now() > deadline {
			anyhow::bail!("enrollment timed out (is the relay reachable and the code fresh?)");
		}
		if status_rx.changed().await.is_err() {
			anyhow::bail!("enrollment task ended unexpectedly");
		}
	}
}

/// Placeholder handler for the enroll-only connection: the phone
/// can't route calls to an IDE with no registered workspaces, so
/// nothing should ever reach it.
struct NullRpc;

#[async_trait::async_trait]
impl moon_remote::rpc::BridgeRpcHandler for NullRpc {
	async fn dispatch(&self, _method: &str, _params: serde_json::Value) -> Result<serde_json::Value, String> {
		Err("not serving yet (enroll-only connection)".into())
	}
	async fn subscribe(
		&self,
		_method: &str,
		_params: serde_json::Value,
	) -> Result<tokio::sync::mpsc::Receiver<serde_json::Value>, String> {
		Err("not serving yet (enroll-only connection)".into())
	}
}

/// Create a workspace catalog entry + bind one folder, mirroring the
/// desktop's create-workspace + open-folder flow closely enough that
/// the desktop (and `serve`) can open the result.
async fn workspace_add(name: String, folder: Utf8PathBuf, slug: Option<String>) -> anyhow::Result<()> {
	let name = name.trim().to_string();
	anyhow::ensure!(!name.is_empty(), "workspace name must not be empty");
	anyhow::ensure!(folder.is_absolute(), "--folder must be an absolute path");
	anyhow::ensure!(folder.is_dir(), "no directory at {folder}");
	let slug = match slug {
		Some(s) => s,
		None => name
			.to_lowercase()
			.chars()
			.map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
			.collect::<String>()
			.split('-')
			.filter(|s| !s.is_empty())
			.collect::<Vec<_>>()
			.join("-"),
	};
	moon_protocol::workspace::validate_workspace_id(&slug)?;
	let config_dir = config_dir()?;
	let (_, workspaces_dir) = data_dirs()?;
	let now = std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map(|d| d.as_secs() as i64)
		.unwrap_or(0);
	let slug_for_catalog = slug.clone();
	let name_for_catalog = name.clone();
	app_state_store::mutate(&config_dir, move |s| {
		if let Some(existing) = s.workspaces.iter_mut().find(|m| m.id == slug_for_catalog) {
			existing.name = name_for_catalog;
			existing.last_active_at = now;
		} else {
			s.workspaces.push(moon_protocol::workspace::WorkspaceMeta {
				id: slug_for_catalog,
				name: name_for_catalog,
				color: None,
				last_active_at: now,
			});
		}
	})
	.await?;
	// Bind the folder in session.json (load-then-save keeps any
	// existing fields when re-running against an existing slug).
	let mut session = core_session::load(&workspaces_dir, &slug).await.unwrap_or_default();
	if !session.folders.iter().any(|f| f.folder_path == folder.as_str()) {
		session.folders.push(moon_protocol::session::FolderSession {
			folder_path: folder.to_string(),
			..Default::default()
		});
	}
	session.active_folder_path = Some(folder.to_string());
	core_session::save(&workspaces_dir, &slug, &session).await?;
	println!("Workspace `{slug}` ready with folder {folder}. Run `moon-remote serve --workspace {slug}`.");
	Ok(())
}

/// Unbind one folder from a workspace's `session.json`. Worktree
/// folders whose parent is the removed folder are left alone — they
/// belong to coordinator workers and get cleaned up through
/// `discard_worker_worktree`.
async fn workspace_remove_folder(workspace: String, folder: Utf8PathBuf) -> anyhow::Result<()> {
	moon_protocol::workspace::validate_workspace_id(&workspace)?;
	let (_, workspaces_dir) = data_dirs()?;
	let mut session = core_session::load(&workspaces_dir, &workspace)
		.await
		.map_err(|e| anyhow::anyhow!("no session state for workspace `{workspace}`: {e}"))?;
	let before = session.folders.len();
	session.folders.retain(|f| f.folder_path != folder.as_str());
	anyhow::ensure!(
		session.folders.len() < before,
		"`{folder}` is not a bound folder of `{workspace}` (bound: {})",
		session
			.folders
			.iter()
			.map(|f| f.folder_path.as_str())
			.collect::<Vec<_>>()
			.join(", ")
	);
	if session.active_folder_path.as_deref() == Some(folder.as_str()) {
		session.active_folder_path = session.folders.first().map(|f| f.folder_path.clone());
	}
	core_session::save(&workspaces_dir, &workspace, &session).await?;
	println!(
		"Removed {folder} from `{workspace}` ({} folder(s) remain).",
		session.folders.len()
	);
	println!(
		"(if the workspace's `serve` was running, restart it — and note it may have clobbered this edit on shutdown)"
	);
	Ok(())
}

/// List / toggle MCP servers in a workspace's `session.json` —
/// headless twin of the desktop's MCP settings panel. Toggling
/// mirrors `coder_mcp_set_enabled`'s rules: the id must be a preset
/// or an existing custom entry.
async fn mcp(workspace: String, enable: Option<String>, disable: Option<String>) -> anyhow::Result<()> {
	moon_protocol::workspace::validate_workspace_id(&workspace)?;
	let (_, workspaces_dir) = data_dirs()?;
	let mut session = core_session::load(&workspaces_dir, &workspace)
		.await
		.map_err(|e| anyhow::anyhow!("no session state for workspace `{workspace}`: {e}"))?;
	let mut changed = false;
	for (id, want) in [(enable, true), (disable, false)] {
		let Some(id) = id else {
			continue;
		};
		let known = moon_coder::mcp::server_rows(&session.coder_mcp);
		anyhow::ensure!(
			known.iter().any(|row| row.config.id == id),
			"unknown MCP server `{id}` (known: {})",
			known
				.iter()
				.map(|r| r.config.id.as_str())
				.collect::<Vec<_>>()
				.join(", ")
		);
		let currently = session.coder_mcp.enabled.iter().any(|e| e == &id);
		if want && !currently {
			session.coder_mcp.enabled.push(id);
			changed = true;
		} else if !want && currently {
			session.coder_mcp.enabled.retain(|e| e != &id);
			changed = true;
		}
	}
	if changed {
		core_session::save(&workspaces_dir, &workspace, &session).await?;
	}
	for row in moon_coder::mcp::server_rows(&session.coder_mcp) {
		let mark = if session.coder_mcp.enabled.iter().any(|e| e == &row.config.id) {
			"enabled "
		} else {
			"disabled"
		};
		let kind = if row.preset { "preset" } else { "custom" };
		println!("{mark}  {:<12} ({kind})  {}", row.config.id, row.config.description);
	}
	if changed {
		println!("(restart the workspace's `serve` to pick this up — and only toggle while it's stopped)");
	}
	Ok(())
}

/// Spawns a detached `moon-remote serve --workspace <id>` — the
/// headless flavour of the desktop's focus-or-spawn `window_open`.
/// If the workspace is already owned, the child loses the instance
/// bind and exits; harmless.
struct HeadlessLauncher;

#[async_trait::async_trait]
impl WorkspaceLauncher for HeadlessLauncher {
	async fn launch(&self, workspace_id: &str) -> Result<(), String> {
		let exe = std::env::current_exe().map_err(|e| format!("could not resolve own binary: {e}"))?;
		std::process::Command::new(exe)
			.args(["serve", "--workspace", workspace_id])
			.stdin(std::process::Stdio::null())
			.stdout(std::process::Stdio::null())
			.stderr(std::process::Stdio::null())
			.spawn()
			.map_err(|e| format!("could not spawn workspace process: {e}"))?;
		Ok(())
	}
}

/// Boot the coder for `slug` and serve it through the relay until
/// SIGINT/SIGTERM. Mirrors the desktop's setup subset: state load,
/// registry + folder restore, models seed, `CoderHandle`, relay
/// connect with the full catalog registered.
/// Shell-container lifecycle for a headless workspace: the same
/// moon-container `setup`/`stop`/`status` path the desktop's
/// container panel drives. `--up` is idempotent (regenerate compose
/// from the current bound folders, `up -d --wait`), so it can run
/// from an `ExecStartPre=` or after folder changes. A running
/// `serve` picks the container up on the next tool call — routing
/// probes live status per call, no restart needed.
async fn container(slug: String, up: bool, stop: bool, image: Option<String>) -> anyhow::Result<()> {
	anyhow::ensure!(!(up && stop), "--up and --stop are mutually exclusive");
	moon_protocol::workspace::validate_workspace_id(&slug)?;
	let (_, workspaces_dir) = data_dirs()?;
	let session = core_session::load(&workspaces_dir, &slug).await.unwrap_or_default();
	anyhow::ensure!(
		!session.folders.is_empty(),
		"workspace `{slug}` has no bound folders — run `moon-remote workspace-add` first"
	);
	// Worktree-backed folders are host-only (ADR 0028), same
	// filter as the desktop's `workspace_handle`.
	let bound_folders: Vec<Utf8PathBuf> = session
		.folders
		.iter()
		.filter(|f| !matches!(f.origin, moon_protocol::workspace::FolderOrigin::Worktree { .. }))
		.map(|f| Utf8PathBuf::from(&f.folder_path))
		.collect();
	// The ssh-agent proxy must be registered in-process before any
	// compose render so `detect_ssh_agent_forward` prefers it —
	// same rationale as `serve`.
	{
		let (_, ws_dir) = data_dirs()?;
		moon_container::agent_proxy::spawn(ws_dir.parent().expect("workspaces dir has a parent"))
			.await
			.map_err(|err| anyhow::anyhow!("could not start ssh-agent proxy: {err}"))?;
	}
	let ws = moon_container::Workspace::new(moon_container::WorkspaceConfig {
		workspace_id: slug.clone(),
		state_dir: workspaces_dir.join(&slug),
		bound_folders,
	})?;
	if up {
		let image = image.as_deref().unwrap_or(moon_container::DEFAULT_DEV_IMAGE);
		println!("bringing up shell container for `{slug}` (image {image})…");
		ws.setup(image).await?;
	} else if stop {
		ws.stop().await?;
	}
	let status = ws.status().await?;
	println!("state: {:?}", status.state);
	for svc in &status.services {
		println!("  {} — {}", svc.name, svc.raw_state);
	}
	Ok(())
}

async fn serve(slug: String) -> anyhow::Result<()> {
	moon_protocol::workspace::validate_workspace_id(&slug)?;
	// Host-side ssh-agent proxy (ADR 0060), same as the desktop's
	// setup callback: a stable socket directory the compose layer
	// prefers over a raw `$SSH_AUTH_SOCK` mount, so host agent
	// restarts never leave the container with a stale bind-mount.
	// Without this, a headless host renders compose with **no**
	// ssh-agent block at all (the service env carries no
	// SSH_AUTH_SOCK) and every container grew up unable to ssh.
	{
		let (_, workspaces_dir) = data_dirs()?;
		moon_container::agent_proxy::spawn(workspaces_dir.parent().expect("workspaces dir has a parent"))
			.await
			.map_err(|err| anyhow::anyhow!("could not start ssh-agent proxy: {err}"))?;
	}
	let config_dir = config_dir()?;
	let (data_dir, workspaces_dir) = data_dirs()?;
	let coder_sessions_dir = data_dir.join("coder-sessions");
	let folder_summaries_dir = data_dir.join("folder-summaries");

	// Single-instance lock (ADR 0014): own the workspace or bail.
	// Connections queue in the backlog until the workspace has booted
	// and the accept loop starts.
	let listener = instance_bind(&workspaces_dir, &slug).await?;

	// Bump last-active + load the catalog & coder defaults.
	let slug_for_bump = slug.clone();
	let now = std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map(|d| d.as_secs() as i64)
		.unwrap_or(0);
	let loaded_state = app_state_store::mutate(&config_dir, move |s| {
		if let Some(meta) = s.workspaces.iter_mut().find(|m| m.id == slug_for_bump) {
			meta.last_active_at = now;
		}
		s.clone()
	})
	.await?;
	anyhow::ensure!(
		loaded_state.workspaces.iter().any(|m| m.id == slug),
		"unknown workspace `{slug}` — run `moon-remote workspace-add` (or create it in the desktop) first"
	);

	let registry = moon_app::boot::new_registry(slug.clone(), workspaces_dir.clone());
	let logs = moon_core::LogSink::new();
	registry.set_log_sink(logs.clone());
	let session = core_session::load(&workspaces_dir, &slug).await.unwrap_or_default();
	anyhow::ensure!(
		!session.folders.is_empty(),
		"workspace `{slug}` has no bound folders — run `moon-remote workspace-add --slug {slug} --folder <path>`"
	);
	moon_app::boot::restore_folders(&registry, &session).await;

	let terminals = Arc::new(moon_terminal::TerminalRegistry::default());
	let coder = moon_coder::CoderHandle::new(
		registry.clone(),
		workspaces_dir.clone(),
		coder_sessions_dir,
		folder_summaries_dir,
		moon_app::boot::coder_models(&loaded_state, session.coder_provider_lock.as_ref()),
		terminals.clone(),
	)
	.map_err(|err| anyhow::anyhow!("could not init moon-coder: {err}"))?;
	coder.spawn_prime_context_windows();

	// The full desktop surface (ADR 0095), delivered to whichever
	// desktop is attached.
	let hub = Arc::new(moon_app::attach::AttachHub::default());
	let events: Arc<dyn moon_app::EventSink> = hub.clone();
	let fs_watcher = moon_app::fs_watcher::spawn(events.clone());
	fs_watcher.set_root(registry.snapshot().await.active_folder.map(std::path::PathBuf::from));
	let core = Arc::new(moon_app::AppCore {
		workspaces: registry.clone(),
		config_dir: config_dir.clone(),
		workspaces_dir: workspaces_dir.clone(),
		workspace_id: Some(slug.clone()),
		coder: coder.clone(),
		events,
		preview_tunnels: moon_container::PreviewTunnels::new(),
		logs,
		terminals,
		terminal_streams: Default::default(),
		lsp: Default::default(),
		log_streams: Default::default(),
		fs_watcher,
	});
	core.spawn_pumps();
	core.install_agent_terminals();
	tokio::spawn(moon_app::container_events::watch(core.clone()));
	{
		let core = core.clone();
		tokio::spawn(async move {
			// Shell first: per-folder composes attach to its network.
			core.auto_resume_shell().await;
			core.auto_resume_project_composes().await;
		});
	}

	// Persist folder binds/unbinds as they happen. An attached
	// desktop saves session state itself, but while none is —
	// without this, folders a coordinator binds at runtime
	// (`clone_repo`, `init_repo`, worker worktrees) evaporate on
	// restart. `WorkspaceFoldersChanged` fires on every such
	// mutation; the reconcile keeps existing per-folder UI fields
	// (open tabs, etc.) for folders that survive.
	{
		let mut events = coder.subscribe();
		let registry = registry.clone();
		let workspaces_dir = workspaces_dir.clone();
		let slug = slug.clone();
		tokio::spawn(async move {
			loop {
				match events.recv().await {
					Ok(envelope) => {
						if !matches!(envelope.event, moon_coder::CoderEvent::WorkspaceFoldersChanged) {
							continue;
						}
						let snapshot = registry.snapshot().await;
						let mut session = core_session::load(&workspaces_dir, &slug).await.unwrap_or_default();
						session.folders = snapshot
							.folders
							.iter()
							.map(|f| {
								let mut entry = session
									.folders
									.iter()
									.find(|e| e.folder_path == f.path)
									.cloned()
									.unwrap_or_else(|| moon_protocol::session::FolderSession {
										folder_path: f.path.clone(),
										..Default::default()
									});
								entry.origin = f.origin.clone();
								entry
							})
							.collect();
						session.active_folder_path = snapshot.active_folder.clone();
						if let Err(err) = core_session::save(&workspaces_dir, &slug, &session).await {
							tracing::warn!(error = %err, "failed to persist folder change to session.json");
						}
					}
					Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
					Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
				}
			}
		});
	}

	// The phone path is optional: a server that never enrolled with a
	// relay still serves attached desktops.
	let pair_ctx: Arc<tokio::sync::OnceCell<PairCtx>> = Arc::new(tokio::sync::OnceCell::new());
	let mut relay_status = match relay::load_credential()? {
		Some(cred) => Some(start_relay(
			cred,
			&slug,
			&loaded_state,
			&coder,
			&registry,
			&config_dir,
			&workspaces_dir,
			&pair_ctx,
		)),
		None => {
			tracing::info!("not enrolled with a relay; serving attached desktops only");
			None
		}
	};

	let _instance = serve_instance(listener, Arc::clone(&pair_ctx), core.clone(), hub);
	tracing::info!(workspace = %slug, "moon-remote serving");

	let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
	loop {
		tokio::select! {
			_ = tokio::signal::ctrl_c() => break,
			_ = terminate.recv() => break,
			changed = async {
				match relay_status.as_mut() {
					Some(rx) => rx.changed().await,
					None => std::future::pending().await,
				}
			} => {
				let Some(rx) = relay_status.as_ref() else {
					continue;
				};
				if changed.is_err() {
					anyhow::bail!("relay connection task ended unexpectedly");
				}
				let status = rx.borrow().clone();
				match (&status.connected, &status.error) {
					(true, _) => tracing::info!(phones = status.connected_phones, "relay connected"),
					(false, Some(err)) => tracing::warn!(error = %err, "relay disconnected; retrying"),
					(false, None) => tracing::info!("relay connecting…"),
				}
			}
		}
	}
	tracing::info!("shutting down");
	core.stop_all().await;
	Ok(())
}

/// Connect to the enrolled relay so phones can drive this workspace
/// (ADR 0059). Returns the connection-status receiver.
#[allow(clippy::too_many_arguments)]
fn start_relay(
	cred: relay::RemoteBridgeCredential,
	slug: &str,
	loaded_state: &moon_protocol::app_state::AppState,
	coder: &moon_coder::CoderHandle,
	registry: &Arc<WorkspaceRegistry>,
	config_dir: &Utf8Path,
	workspaces_dir: &Utf8Path,
	pair_ctx: &tokio::sync::OnceCell<PairCtx>,
) -> tokio::sync::watch::Receiver<relay::RemoteBridgeStatus> {
	let plain_rpc: Arc<dyn moon_remote::rpc::BridgeRpcHandler> = Arc::new(BridgeRpc::new(
		coder.clone(),
		registry.clone(),
		SettingsContext {
			config_dir: config_dir.to_owned(),
			workspaces_dir: workspaces_dir.to_owned(),
			workspace_id: Some(slug.to_owned()),
		},
		Some(Arc::new(HeadlessLauncher)),
	));
	// Relay path is strict (ADR 0087): only end-to-end sealed calls
	// from phones this host paired itself reach the dispatcher.
	let e2e = Arc::new(moon_remote::e2e::E2eEndpoint::keyring(
		slug.to_owned(),
		moon_remote::e2e::host_label(),
	));
	let rpc: Arc<dyn moon_remote::rpc::BridgeRpcHandler> =
		Arc::new(moon_remote::e2e::E2eRpc::new(plain_rpc, Arc::clone(&e2e), true));

	// Register the full catalog with this workspace marked live —
	// same shape the desktop sends — so the phone's switcher shows
	// stopped workspaces as launchable.
	let workspaces: Vec<relay::RemoteWorkspace> = loaded_state
		.workspaces
		.iter()
		.map(|m| relay::RemoteWorkspace {
			id: m.id.clone(),
			name: m.name.clone(),
			last_active_at: Some(m.last_active_at),
			live: m.id == slug,
		})
		.collect();

	let handle = Arc::new(relay::spawn(
		cred.bridge_url.clone(),
		String::new(),
		cred.ide_id.clone(),
		cred.ide_id.clone(),
		workspaces,
		rpc,
	));
	let status = handle.status_receiver();
	let _ = pair_ctx.set(PairCtx {
		handle,
		e2e,
		ide_id: cred.ide_id.clone(),
	});
	tracing::info!(bridge = %cred.bridge_url, ide_id = %cred.ide_id, "relay enabled");
	status
}

/// Show or persist the model picks. Writes the same `state.json`
/// fields the desktop picker saves, so the two stay interchangeable;
/// prints the effective values (empty = built-in default) either way.
async fn model(
	standard: Option<String>,
	cheap: Option<String>,
	rotation: Option<String>,
	reasoning: Option<String>,
) -> anyhow::Result<()> {
	let config_dir = config_dir()?;
	let state = app_state_store::mutate(&config_dir, move |s| {
		if let Some(standard) = standard {
			s.coder.standard_model = standard.trim().to_string();
		}
		if let Some(cheap) = cheap {
			s.coder.cheap_model = cheap.trim().to_string();
		}
		if let Some(reasoning) = reasoning {
			let cleaned = reasoning.trim().to_string();
			s.coder.reasoning_effort = if cleaned.is_empty() { None } else { Some(cleaned) };
		}
		if let Some(rotation) = rotation {
			s.coder.rotation = rotation
				.split(',')
				.map(str::trim)
				.filter(|m| !m.is_empty())
				.map(str::to_owned)
				.collect();
		}
		s.clone()
	})
	.await?;
	let show = |slug: &str, default: &str| {
		if slug.is_empty() {
			format!("{default} (default)")
		} else {
			slug.to_string()
		}
	};
	println!(
		"standard  {}",
		show(
			&state.coder.standard_model,
			moon_coder::defaults::DEFAULT_STANDARD_MODEL
		)
	);
	println!(
		"cheap     {}",
		show(&state.coder.cheap_model, moon_coder::defaults::DEFAULT_CHEAP_MODEL)
	);
	if state.coder.rotation.is_empty() {
		println!("rotation  (none)");
	} else {
		println!("rotation  {}", state.coder.rotation.join(" -> "));
	}
	match &state.coder.reasoning_effort {
		Some(effort) => println!("reasoning {effort}"),
		None => println!("reasoning (provider default — field not sent)"),
	}
	println!("(a running `moon-remote serve` picks these up on restart)");
	Ok(())
}

/// What a running `serve` needs to answer `moon-remote pair`.
struct PairCtx {
	handle: Arc<relay::RemoteBridgeHandle>,
	e2e: Arc<moon_remote::e2e::E2eEndpoint>,
	ide_id: String,
}

impl PairCtx {
	async fn link(&self) -> anyhow::Result<String> {
		let qr = self.handle.request_pair_code().await?;
		self.e2e.pair_link_for(&qr.url, &qr.code, &self.ide_id)
	}
}

/// Opening request on `instance.sock`: `{"op":"attach"}` or `{"op":"pair"}`.
#[derive(serde::Deserialize)]
struct InstanceRequest {
	op: String,
}

fn instance_socket_path(workspaces_dir: &Utf8Path, slug: &str) -> Utf8PathBuf {
	workspaces_dir.join(slug).join("run").join("instance.sock")
}

/// Bind the per-workspace single-instance socket (same path +
/// stale-recovery semantics as the desktop's `focus_socket::try_bind`).
async fn instance_bind(workspaces_dir: &Utf8Path, slug: &str) -> anyhow::Result<tokio::net::UnixListener> {
	let path = instance_socket_path(workspaces_dir, slug);
	tokio::fs::create_dir_all(path.parent().expect("socket path has a parent")).await?;
	match tokio::net::UnixListener::bind(path.as_std_path()) {
		Ok(l) => Ok(l),
		Err(err) if err.kind() == std::io::ErrorKind::AddrInUse => {
			// Live owner or stale debris? Probe: only a real listener
			// accepts.
			let probe = tokio::time::timeout(
				std::time::Duration::from_millis(500),
				tokio::net::UnixStream::connect(path.as_std_path()),
			)
			.await;
			if matches!(probe, Ok(Ok(_))) {
				anyhow::bail!("workspace `{slug}` is already being served by another process");
			}
			tokio::fs::remove_file(path.as_std_path()).await?;
			Ok(tokio::net::UnixListener::bind(path.as_std_path())?)
		}
		Err(err) => Err(err.into()),
	}
}

/// Answer `instance.sock`: each connection opens with one JSON request
/// line. `{"op":"attach"}` turns the connection into the attached
/// desktop's channel (ADR 0095); `{"op":"pair"}` answers a local
/// `moon-remote pair`. Liveness probes connect and drop. Same user
/// only — the socket lives in the user's data dir.
fn serve_instance(
	listener: tokio::net::UnixListener,
	pair_ctx: Arc<tokio::sync::OnceCell<PairCtx>>,
	core: Arc<moon_app::AppCore>,
	hub: Arc<moon_app::attach::AttachHub>,
) -> tokio::task::JoinHandle<()> {
	tokio::spawn(async move {
		while let Ok((stream, _)) = listener.accept().await {
			let pair_ctx = Arc::clone(&pair_ctx);
			let core = core.clone();
			let hub = hub.clone();
			tokio::spawn(async move {
				use tokio::io::AsyncWriteExt;
				let (mut read, mut write) = stream.into_split();
				let line = tokio::time::timeout(std::time::Duration::from_secs(5), read_request_line(&mut read)).await;
				let Ok(Some(line)) = line else {
					return;
				};
				let Ok(req) = serde_json::from_str::<InstanceRequest>(line.trim()) else {
					return;
				};
				let reply = match (req.op.as_str(), pair_ctx.get()) {
					("attach", _) => {
						moon_app::attach::serve_connection(core, hub, read, write).await;
						return;
					}
					("pair", Some(ctx)) => match ctx.link().await {
						Ok(link) => serde_json::json!({ "link": link }),
						Err(err) => serde_json::json!({ "error": err.to_string() }),
					},
					("pair", None) => serde_json::json!({ "error": "this workspace isn't enrolled with a relay" }),
					_ => return,
				};
				let _ = write.write_all(format!("{reply}\n").as_bytes()).await;
			});
		}
	})
}

/// Read the opening request line byte by byte, so nothing after it is
/// swallowed into a buffer: for `attach` the rest of the stream is the
/// desktop's frames.
async fn read_request_line(read: &mut tokio::net::unix::OwnedReadHalf) -> Option<String> {
	use tokio::io::AsyncReadExt;
	let mut line = Vec::new();
	loop {
		let byte = read.read_u8().await.ok()?;
		if byte == b'\n' {
			return String::from_utf8(line).ok();
		}
		if line.len() >= 4096 {
			return None;
		}
		line.push(byte);
	}
}

/// `moon-remote attach`: bridge stdio to the workspace process's
/// `instance.sock`, launching `serve` detached when nothing answers.
async fn attach(slug: String) -> anyhow::Result<()> {
	use tokio::io::AsyncWriteExt;
	moon_protocol::workspace::validate_workspace_id(&slug)?;
	let (_, workspaces_dir) = data_dirs()?;
	let path = instance_socket_path(&workspaces_dir, &slug);
	let stream = match tokio::net::UnixStream::connect(path.as_std_path()).await {
		Ok(stream) => stream,
		Err(_) => {
			launch_serve(&workspaces_dir, &slug)?;
			wait_for_socket(&path).await?
		}
	};
	let (mut sock_read, mut sock_write) = stream.into_split();
	sock_write.write_all(b"{\"op\":\"attach\"}\n").await?;
	let mut stdin = tokio::io::stdin();
	let mut stdout = tokio::io::stdout();
	// Whichever side closes first ends the session: the desktop going
	// away, or the server detaching us for a newer attachment.
	tokio::select! {
		_ = tokio::io::copy(&mut stdin, &mut sock_write) => {}
		_ = async {
			let _ = tokio::io::copy(&mut sock_read, &mut stdout).await;
			let _ = stdout.flush().await;
		} => {}
	}
	Ok(())
}

/// Start `moon-remote serve` in its own process group so it outlives
/// this `attach` (and the ssh session that ran it). Output goes to
/// `<workspace>/run/serve.log`.
fn launch_serve(workspaces_dir: &Utf8Path, slug: &str) -> anyhow::Result<()> {
	use std::os::unix::process::CommandExt;
	let run_dir = workspaces_dir.join(slug).join("run");
	std::fs::create_dir_all(&run_dir)?;
	let log = std::fs::OpenOptions::new()
		.create(true)
		.append(true)
		.open(run_dir.join("serve.log"))?;
	let mut command = std::process::Command::new(std::env::current_exe()?);
	command
		.args(["serve", "--workspace", slug])
		.stdin(std::process::Stdio::null())
		.stdout(log.try_clone()?)
		.stderr(log)
		.process_group(0);
	command.spawn()?;
	Ok(())
}

async fn wait_for_socket(path: &Utf8Path) -> anyhow::Result<tokio::net::UnixStream> {
	let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
	loop {
		if let Ok(stream) = tokio::net::UnixStream::connect(path.as_std_path()).await {
			return Ok(stream);
		}
		if tokio::time::Instant::now() > deadline {
			anyhow::bail!(
				"`moon-remote serve` didn't come up within 60s; see {}",
				path.with_file_name("serve.log")
			);
		}
		tokio::time::sleep(std::time::Duration::from_millis(250)).await;
	}
}

/// `moon-remote pair`: ask the workspace's running `serve` for a
/// pairing link and print it (plus a terminal QR for the ssh case).
async fn pair(slug: String, no_qr: bool) -> anyhow::Result<()> {
	use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
	moon_protocol::workspace::validate_workspace_id(&slug)?;
	let (_, workspaces_dir) = data_dirs()?;
	let path = instance_socket_path(&workspaces_dir, &slug);
	let stream = tokio::net::UnixStream::connect(path.as_std_path())
		.await
		.map_err(|err| anyhow::anyhow!("no running `moon-remote serve --workspace {slug}` ({err})"))?;
	let (read, mut write) = stream.into_split();
	write.write_all(b"{\"op\":\"pair\"}\n").await?;
	let mut line = String::new();
	tokio::time::timeout(
		std::time::Duration::from_secs(20),
		tokio::io::BufReader::new(read).read_line(&mut line),
	)
	.await
	.map_err(|_| anyhow::anyhow!("serve did not answer the pairing request in time"))??;
	let reply: serde_json::Value = serde_json::from_str(line.trim())
		.map_err(|_| anyhow::anyhow!("unexpected reply from serve (is it an older build?)"))?;
	if let Some(err) = reply.get("error").and_then(|v| v.as_str()) {
		anyhow::bail!("{err}");
	}
	let link = reply
		.get("link")
		.and_then(|v| v.as_str())
		.ok_or_else(|| anyhow::anyhow!("serve replied without a link"))?;
	if !no_qr {
		let code = qrcode::QrCode::new(link.as_bytes())?;
		let art = code
			.render::<qrcode::render::unicode::Dense1x2>()
			.dark_color(qrcode::render::unicode::Dense1x2::Light)
			.light_color(qrcode::render::unicode::Dense1x2::Dark)
			.quiet_zone(true)
			.build();
		println!("{art}");
	}
	println!("Open this link on the phone (or scan the QR). Single use, valid 15 minutes:\n\n{link}\n");
	println!("It carries this host's key and a one-time secret — share it only with the phone you're pairing.");
	Ok(())
}

/// `moon-remote devices`: list or revoke end-to-end paired phones.
fn devices(revoke: Option<String>) -> anyhow::Result<()> {
	let e2e = moon_remote::e2e::E2eEndpoint::keyring(String::new(), moon_remote::e2e::host_label());
	if let Some(id) = revoke {
		if e2e.revoke(&id)? {
			println!("revoked {id} (its live sessions end within 30 s)");
		} else {
			anyhow::bail!("no paired phone with id `{id}`");
		}
		return Ok(());
	}
	let list = e2e.devices()?;
	if list.is_empty() {
		println!("no phones paired — run `moon-remote pair --workspace <slug>`");
	}
	for d in list {
		println!("{}  {}  (paired {})", d.id, d.label, d.paired_at_ms);
	}
	Ok(())
}
