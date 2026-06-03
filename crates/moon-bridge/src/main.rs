//! moon-bridge — the host-resident daemon that exposes a running
//! moon-ide's coder + git surface to a mobile companion app over
//! the LAN.
//!
//! Phase 13 (mobile companion). Shipped so far:
//!
//! - 13.0 — workspace discovery (`list`): enumerate the per-workspace
//!   `instance.sock` files moon-ide maintains (ADR 0014) and report
//!   which workspaces are running.
//! - 13.1 — relay (`call`): invoke a method on a chosen workspace
//!   process over its `instance.sock`, using the `R` (RPC) request
//!   kind on the `moon-remote`-style JSON shape (ADR 0023).
//! - 13.3 (core) — pairing (`pair` / `devices` / `revoke`): mint and
//!   store revocable per-device bearer tokens in the OS keyring.
//!
//! Still to come: the LAN HTTPS + WebSocket listener with TLS (13.2)
//! and the companion PWA (13.4 / 13.5).
//!
//! See [`specs/companion.md`](../../../specs/companion.md),
//! [`specs/roadmaps/phase-13-mobile-companion.md`](../../../specs/roadmaps/phase-13-mobile-companion.md),
//! and [ADR 0023](../../../specs/decisions/0023-mobile-companion-bridge.md).

mod discovery;
mod pairing;
mod relay;

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "moon-bridge", version, about = "moon-ide mobile companion bridge")]
struct Args {
	#[command(subcommand)]
	command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
	/// List the moon-ide workspaces found on this machine and whether
	/// each one is currently running.
	List {
		/// Emit machine-readable JSON instead of the human table.
		#[arg(long)]
		json: bool,
	},
	/// Invoke a method on a running workspace over its instance.sock.
	/// This is the relay the WSS listener will sit in front of.
	Call {
		/// Workspace slug (as shown by `list`).
		workspace: String,
		/// Method name (e.g. `coder_status`, `coder_list_sessions`,
		/// `workspace_snapshot`, `bridge_methods`).
		method: String,
		/// Optional JSON params object. Defaults to `{}`.
		#[arg(long, default_value = "{}")]
		params: String,
	},
	/// Pair a new device: mint a bearer token and store it in the
	/// keyring. Prints the token once — it is never recoverable
	/// afterwards.
	Pair {
		/// Human-readable label for the device ("Eli's iPhone").
		label: String,
	},
	/// List paired devices (id, label, paired-at). Tokens are never
	/// printed here.
	Devices,
	/// Revoke a paired device by id (as shown by `devices`).
	Revoke {
		/// Device id to revoke.
		id: String,
	},
	/// Issue a short-lived pairing code (what the desktop will encode
	/// into the pairing QR). Prints the code and its TTL. The verify
	/// half runs in the WSS listener (13.2).
	PairCode,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
	tracing_subscriber::fmt()
		.with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
		.init();

	let args = Args::parse();
	match args.command {
		Command::List { json } => run_list(json).await,
		Command::Call {
			workspace,
			method,
			params,
		} => run_call(&workspace, &method, &params).await,
		Command::Pair { label } => run_pair(&label),
		Command::Devices => run_devices(),
		Command::Revoke { id } => run_revoke(&id),
		Command::PairCode => run_pair_code(),
	}
}

async fn run_list(json: bool) -> anyhow::Result<()> {
	let workspaces_dir = discovery::resolve_workspaces_dir()?;
	let config_dir = discovery::resolve_config_dir()?;
	let found = discovery::discover(&workspaces_dir, &config_dir).await?;

	if json {
		print_json(&found);
		return Ok(());
	}

	print_table(&found, &workspaces_dir);
	Ok(())
}

async fn run_call(workspace: &str, method: &str, params: &str) -> anyhow::Result<()> {
	let params: serde_json::Value =
		serde_json::from_str(params).map_err(|e| anyhow::anyhow!("--params is not valid JSON: {e}"))?;
	let workspaces_dir = discovery::resolve_workspaces_dir()?;
	let socket = discovery::socket_path(&workspaces_dir, workspace);

	let resp = relay::call(&socket, method, params).await?;
	if let Some(error) = resp.error {
		// Print the workspace process's error to stderr and exit
		// non-zero so scripts can tell a method failure from a
		// transport failure.
		eprintln!("error: {error}");
		std::process::exit(1);
	}
	let ok = resp.ok.unwrap_or(serde_json::Value::Null);
	println!(
		"{}",
		serde_json::to_string_pretty(&ok).unwrap_or_else(|_| "null".to_owned())
	);
	Ok(())
}

fn run_pair(label: &str) -> anyhow::Result<()> {
	let store = pairing::DeviceStore::open()?;
	let device = store.add(pairing::PairedDevice::mint(label))?;
	println!("Paired \"{}\" (id {})", device.label, device.id);
	println!();
	println!("Device token (shown once — store it on the device now):");
	println!("  {}", device.token);
	Ok(())
}

fn run_devices() -> anyhow::Result<()> {
	let store = pairing::DeviceStore::open()?;
	let devices = store.list()?;
	if devices.is_empty() {
		println!("No paired devices.");
		return Ok(());
	}
	let id_width = devices.iter().map(|d| d.id.len()).max().unwrap_or(2).max("ID".len());
	let label_width = devices
		.iter()
		.map(|d| d.label.len())
		.max()
		.unwrap_or(5)
		.max("LABEL".len());
	println!("{:<id_width$}  {:<label_width$}  PAIRED-AT-MS", "ID", "LABEL");
	for d in &devices {
		println!("{:<id_width$}  {:<label_width$}  {}", d.id, d.label, d.paired_at_ms);
	}
	Ok(())
}

fn run_revoke(id: &str) -> anyhow::Result<()> {
	let store = pairing::DeviceStore::open()?;
	if store.revoke(id)? {
		println!("Revoked device {id}");
	} else {
		println!("No device with id {id}");
	}
	Ok(())
}

fn run_pair_code() -> anyhow::Result<()> {
	let session = pairing::PairingSession::issue();
	println!("Pairing code: {}", session.code());
	println!("Valid for {} seconds.", pairing::PAIRING_CODE_TTL.as_secs());
	// The session is dropped here: in 13.2 the listener holds it in
	// memory and runs `verify_and_consume` when the phone presents
	// the code. This subcommand just demonstrates issuance.
	Ok(())
}

fn print_json(found: &[discovery::DiscoveredWorkspace]) {
	let rows: Vec<serde_json::Value> = found
		.iter()
		.map(|w| {
			serde_json::json!({
				"id": w.id,
				"name": w.name,
				"last_active_at": w.last_active_at,
				"live": w.live,
			})
		})
		.collect();
	let doc = serde_json::Value::Array(rows);
	println!(
		"{}",
		serde_json::to_string_pretty(&doc).unwrap_or_else(|_| "[]".to_owned())
	);
}

fn print_table(found: &[discovery::DiscoveredWorkspace], workspaces_dir: &camino::Utf8Path) {
	if found.is_empty() {
		println!("No moon-ide workspaces found under {workspaces_dir}");
		return;
	}

	let id_width = found
		.iter()
		.map(|w| w.id.len())
		.max()
		.unwrap_or(2)
		.max("WORKSPACE".len());
	let name_width = found.iter().map(|w| w.name.len()).max().unwrap_or(4).max("NAME".len());

	println!("{:<id_width$}  {:<name_width$}  STATUS", "WORKSPACE", "NAME");
	for w in found {
		let status = if w.live { "running" } else { "stopped" };
		println!("{:<id_width$}  {:<name_width$}  {status}", w.id, w.name);
	}
}
