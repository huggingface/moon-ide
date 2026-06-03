//! moon-bridge — the host-resident daemon that exposes a running
//! moon-ide's coder + git surface to a mobile companion app over
//! the LAN.
//!
//! Phase 13 (mobile companion). This binary currently ships only
//! sub-phase **13.0**: workspace discovery. It enumerates the
//! per-workspace `instance.sock` files moon-ide already maintains
//! (ADR 0014) and reports which workspaces are running, so later
//! sub-phases can relay to a chosen one.
//!
//! - 13.1 — bridge ↔ workspace-process JSON-RPC relay (over the
//!   `moon-remote` framing, per ADR 0023; not a bespoke socket
//!   verb set).
//! - 13.2 — LAN HTTPS + WebSocket listener with self-signed TLS.
//! - 13.3 — TOFU-cert + device-token pairing.
//! - 13.4 / 13.5 — the companion PWA's coder + review surfaces.
//!
//! See [`specs/companion.md`](../../../specs/companion.md),
//! [`specs/roadmaps/phase-13-mobile-companion.md`](../../../specs/roadmaps/phase-13-mobile-companion.md),
//! and [ADR 0023](../../../specs/decisions/0023-mobile-companion-bridge.md).

mod discovery;

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "moon-bridge", version, about = "moon-ide mobile companion bridge")]
struct Args {
	#[command(subcommand)]
	command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
	/// List the moon-ide workspaces found on this machine and
	/// whether each one is currently running. This is the 13.0
	/// acceptance surface — it proves discovery works against real
	/// running IDE processes before any network code exists.
	List {
		/// Emit machine-readable JSON instead of the human table.
		#[arg(long)]
		json: bool,
	},
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
	tracing_subscriber::fmt()
		.with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
		.init();

	let args = Args::parse();
	match args.command {
		Command::List { json } => run_list(json).await,
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

fn print_json(found: &[discovery::DiscoveredWorkspace]) {
	// Hand-rolled rather than deriving `Serialize` on the public
	// type: the JSON shape here is a debug affordance, not a
	// committed wire contract (that lands with 13.1's relay). Keeping
	// it local means evolving it can't accidentally break a consumer.
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
