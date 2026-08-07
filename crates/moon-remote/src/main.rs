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
use moon_core::app_state as app_state_store;
use moon_core::session as core_session;
use moon_core::WorkspaceRegistry;
use moon_remote::relay;
use moon_remote::rpc::{BridgeRpc, WorkspaceLauncher};
use moon_remote::settings::SettingsContext;

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

	tracing_subscriber::fmt()
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
			Command::Serve { workspace } => serve(workspace).await,
			Command::Model { standard, cheap } => model(standard, cheap).await,
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
async fn serve(slug: String) -> anyhow::Result<()> {
	moon_protocol::workspace::validate_workspace_id(&slug)?;
	let cred = relay::load_credential()?
		.ok_or_else(|| anyhow::anyhow!("not enrolled — run `moon-remote enroll --bridge wss://… --code …` first"))?;
	let config_dir = config_dir()?;
	let (data_dir, workspaces_dir) = data_dirs()?;
	let coder_sessions_dir = data_dir.join("coder-sessions");
	let folder_summaries_dir = data_dir.join("folder-summaries");

	// Single-instance lock (ADR 0014): own the workspace or bail.
	// The listener is held for liveness probes (`connect` succeeding
	// = live owner); R/S frames from a *local* moon-bridge are not
	// served headless — the relay is the supported path.
	let _instance = instance_bind(&workspaces_dir, &slug).await?;

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

	let registry = Arc::new(WorkspaceRegistry::new(slug.clone()));

	// Folder restore from session.json — same walk as the desktop's
	// `restore_session`, minus the UI bits.
	let session = core_session::load(&workspaces_dir, &slug).await.unwrap_or_default();
	anyhow::ensure!(
		!session.folders.is_empty(),
		"workspace `{slug}` has no bound folders — run `moon-remote workspace-add --slug {slug} --folder <path>`"
	);
	for folder in &session.folders {
		let path = Utf8PathBuf::from(&folder.folder_path);
		let result = match &folder.origin {
			moon_protocol::workspace::FolderOrigin::Worktree { parent_path, branch } => {
				if registry.folder_for_path(parent_path).await.is_none() {
					tracing::warn!(path = %path, parent = %parent_path, "skipping orphan worktree folder");
					continue;
				}
				registry
					.add_worktree_folder(path.clone(), parent_path.clone(), branch.clone())
					.await
			}
			moon_protocol::workspace::FolderOrigin::UserPicked => registry.add_folder(path.clone()).await,
		};
		if let Err(e) = result {
			tracing::warn!(error = %e, path = %path, "failed to restore folder");
		}
	}
	if let Some(active) = session.active_folder_path.as_ref() {
		if let Err(e) = registry.set_active_folder(active).await {
			tracing::warn!(error = %e, path = %active, "failed to restore active folder");
		}
	}

	// Models seed: per-workspace provider lock beats the global
	// active provider (same resolution as the desktop's setup).
	let effective_active_provider = match &session.coder_provider_lock {
		Some(moon_protocol::coder_models::CoderProviderLock::Hf) => None,
		Some(moon_protocol::coder_models::CoderProviderLock::User { id }) => Some(id.clone()),
		None => loaded_state.coder.active_provider.clone(),
	};
	let initial_coder_models = moon_coder::CoderModels {
		standard: loaded_state.coder.standard_model.clone(),
		cheap: loaded_state.coder.cheap_model.clone(),
		bill_to: if loaded_state.coder.bill_to.is_empty() {
			None
		} else {
			Some(loaded_state.coder.bill_to.clone())
		},
		providers: loaded_state.coder.providers.clone(),
		active_provider: effective_active_provider,
		context_window_overrides: Arc::new(loaded_state.coder.context_window_overrides.clone()),
		..moon_coder::CoderModels::default()
	};

	let terminals = Arc::new(moon_terminal::TerminalRegistry::default());
	let coder = moon_coder::CoderHandle::new(
		registry.clone(),
		workspaces_dir.clone(),
		coder_sessions_dir,
		folder_summaries_dir,
		initial_coder_models,
		terminals,
	)
	.map_err(|err| anyhow::anyhow!("could not init moon-coder: {err}"))?;
	coder.spawn_prime_context_windows();

	let rpc: Arc<dyn moon_remote::rpc::BridgeRpcHandler> = Arc::new(BridgeRpc::new(
		coder.clone(),
		registry.clone(),
		SettingsContext {
			config_dir: config_dir.clone(),
			workspaces_dir: workspaces_dir.clone(),
			workspace_id: Some(slug.clone()),
		},
		Some(Arc::new(HeadlessLauncher)),
	));

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

	let handle = relay::spawn(
		cred.bridge_url.clone(),
		String::new(),
		cred.ide_id.clone(),
		cred.ide_id.clone(),
		workspaces,
		rpc,
	);
	tracing::info!(workspace = %slug, bridge = %cred.bridge_url, ide_id = %cred.ide_id, "moon-remote serving");

	// Log status transitions until we're told to stop.
	let mut status_rx = handle.status_receiver();
	loop {
		tokio::select! {
			_ = tokio::signal::ctrl_c() => {
				tracing::info!("interrupted; shutting down");
				return Ok(());
			}
			changed = status_rx.changed() => {
				if changed.is_err() {
					anyhow::bail!("relay connection task ended unexpectedly");
				}
				let status = status_rx.borrow().clone();
				match (&status.connected, &status.error) {
					(true, _) => tracing::info!(phones = status.connected_phones, "relay connected"),
					(false, Some(err)) => tracing::warn!(error = %err, "relay disconnected; retrying"),
					(false, None) => tracing::info!("relay connecting…"),
				}
			}
		}
	}
}

/// Show or persist the model picks. Writes the same `state.json`
/// fields the desktop picker saves, so the two stay interchangeable;
/// prints the effective values (empty = built-in default) either way.
async fn model(standard: Option<String>, cheap: Option<String>) -> anyhow::Result<()> {
	let config_dir = config_dir()?;
	let state = app_state_store::mutate(&config_dir, move |s| {
		if let Some(standard) = standard {
			s.coder.standard_model = standard.trim().to_string();
		}
		if let Some(cheap) = cheap {
			s.coder.cheap_model = cheap.trim().to_string();
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
	println!("(a running `moon-remote serve` picks these up on restart)");
	Ok(())
}

/// Bind the per-workspace single-instance socket (same path +
/// stale-recovery semantics as the desktop's `focus_socket::try_bind`)
/// and hold it, accepting-and-dropping connections so sibling
/// liveness probes see a live owner.
async fn instance_bind(workspaces_dir: &Utf8Path, slug: &str) -> anyhow::Result<tokio::task::JoinHandle<()>> {
	let path = workspaces_dir.join(slug).join("run").join("instance.sock");
	tokio::fs::create_dir_all(path.parent().expect("socket path has a parent")).await?;
	let listener = match tokio::net::UnixListener::bind(path.as_std_path()) {
		Ok(l) => l,
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
			tokio::net::UnixListener::bind(path.as_std_path())?
		}
		Err(err) => return Err(err.into()),
	};
	Ok(tokio::spawn(async move {
		// Accept + drop: liveness probes succeed, everything else
		// (focus/edit/RPC frames from a local bridge) is refused by
		// the close. Headless serves through the relay only.
		while let Ok((_stream, _)) = listener.accept().await {}
	}))
}
