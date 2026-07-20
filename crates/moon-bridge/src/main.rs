//! moon-bridge — the host-resident daemon that exposes a running
//! moon-ide's coder + git surface to a mobile companion app over
//! the LAN.
//!
//! Phase 13 (mobile companion) + Phase 14 (remote / relay bridge),
//! both landed:
//!
//! - 13.0 — workspace discovery (`list`): enumerate the per-workspace
//!   `instance.sock` files moon-ide maintains (ADR 0014) and report
//!   which workspaces are running.
//! - 13.1 — relay (`call`): invoke a method on a chosen workspace
//!   process over its `instance.sock`, using the `R` (RPC) request
//!   kind on the `moon-remote`-style JSON shape (ADR 0023).
//! - 13.2 / 13.4 — the LAN HTTPS + WebSocket listener with TLS
//!   (`serve`), serving the companion PWA and its data channel.
//! - 13.3 — pairing (`pair` / `devices` / `revoke`): mint and store
//!   revocable per-device bearer tokens in the OS keyring.
//! - 14.0–14.2 — IDE enrollment (`enroll-code` / `ides` /
//!   `revoke-ide`) and the remote-relay wiring: the bridge accepts
//!   enrolled IDEs over WSS and forwards `call`/`subscribe` to them
//!   (ADR 0031). A standing relay deployment behind a
//!   TLS-terminating proxy uses `serve --no-idle-exit
//!   --advertise-url` (ADR 0035).
//!
//! See [`specs/companion.md`](../../../specs/companion.md),
//! [`specs/roadmaps/phase-13-mobile-companion.md`](../../../specs/roadmaps/phase-13-mobile-companion.md),
//! and [ADR 0023](../../../specs/decisions/0023-mobile-companion-bridge.md).

mod discovery;
mod enrollment;
mod http;
mod mdns;
mod pairing;
mod relay;
mod serve;
mod status;
mod tls;

/// Default LAN port. IANA dynamic range, adjacent to the next-edit
/// server's 53281 so the two moon-ide listeners cluster.
const DEFAULT_PORT: u16 = 53180;

use std::net::{IpAddr, SocketAddr};

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
	/// Issue a short-lived enrollment code for an IDE to present when
	/// connecting to this bridge as a remote relay (Phase 14, ADR
	/// 0031). Mirror of `pair-code` for the IDE↔bridge relationship.
	/// Prints the code and its TTL; the verify half runs in the WSS
	/// listener (14.1).
	EnrollCode,
	/// List enrolled IDEs (id, label, enrolled-at). Tokens are never
	/// printed here. Mirror of `devices`.
	Ides,
	/// Revoke an enrolled IDE by id (as shown by `ides`). Mirror of
	/// `revoke`.
	RevokeIde {
		/// IDE id to revoke.
		id: String,
	},
	/// Run the LAN WSS listener. Issues an enrollment code at startup;
	/// phone-pairing codes are minted on demand from the IDE (local
	/// panel or enrolled remote IDE). Serves until killed.
	Serve {
		/// Bind address. Defaults to `0.0.0.0:<DEFAULT_PORT>`.
		#[arg(long)]
		bind: Option<SocketAddr>,
		/// LAN host the QR advertises (`wss://<host>:<port>`).
		/// Defaults to the first non-loopback IPv4 address found,
		/// falling back to `127.0.0.1` for a local-only test.
		#[arg(long)]
		advertise_host: Option<String>,
		/// Start with enrollment closed (only already-enrolled IDEs
		/// can connect). Default is to open an enrollment window at
		/// startup (Phase 14, ADR 0031).
		#[arg(long)]
		no_enrollment: bool,
		/// Directory of built PWA assets to serve over HTTPS (the
		/// companion's `companion/dist`). Omit to run WS-only.
		#[arg(long)]
		web_root: Option<std::path::PathBuf>,
		/// Full `wss://…` URL to advertise in the pairing payload,
		/// overriding the `wss://<host>:<port>` built from the bind
		/// address. For a bridge behind a TLS-terminating reverse
		/// proxy (ADR 0035), where the public URL differs from the
		/// local bind.
		#[arg(long)]
		advertise_url: Option<String>,
		/// Keep serving even when no local workspace is live. A
		/// relay-mode bridge (ADR 0031/0035) has no local
		/// `instance.sock`s at all — without this it would exit
		/// seconds after start. Local auto-spawned bridges (ADR 0024)
		/// must NOT set this, or they'd linger after the last IDE
		/// closes.
		#[arg(long)]
		no_idle_exit: bool,
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
		Command::Call {
			workspace,
			method,
			params,
		} => run_call(&workspace, &method, &params).await,
		Command::Pair { label } => run_pair(&label),
		Command::Devices => run_devices(),
		Command::Revoke { id } => run_revoke(&id),
		Command::EnrollCode => run_enroll_code(),
		Command::Ides => run_ides(),
		Command::RevokeIde { id } => run_revoke_ide(&id),
		Command::Serve {
			bind,
			advertise_host,
			no_enrollment,
			web_root,
			advertise_url,
			no_idle_exit,
		} => {
			run_serve(ServeArgs {
				bind,
				advertise_host,
				no_enrollment,
				web_root,
				advertise_url,
				no_idle_exit,
			})
			.await
		}
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

/// Issue a short-lived enrollment code for an IDE (Phase 14.0). Mirror
/// of `run_pair_code` for the IDE↔bridge relationship. The session is
/// dropped here: the `serve` listener holds its own session in memory
/// and runs `verify_and_consume` when an IDE presents the code over
/// WSS. This subcommand just demonstrates issuance.
fn run_enroll_code() -> anyhow::Result<()> {
	let session = enrollment::EnrollmentSession::issue();
	println!("Enrollment code: {}", session.code());
	println!("Valid for {} seconds.", enrollment::ENROLLMENT_CODE_TTL.as_secs());
	Ok(())
}

/// List enrolled IDEs (Phase 14.0). Mirror of `run_devices`.
fn run_ides() -> anyhow::Result<()> {
	let store = enrollment::IdeStore::open()?;
	let ides = store.list()?;
	if ides.is_empty() {
		println!("No enrolled IDEs.");
		return Ok(());
	}
	let id_width = ides.iter().map(|i| i.id.len()).max().unwrap_or(2).max("ID".len());
	let label_width = ides.iter().map(|i| i.label.len()).max().unwrap_or(5).max("LABEL".len());
	println!("{:<id_width$}  {:<label_width$}  ENROLLED-AT-MS", "ID", "LABEL");
	for i in &ides {
		println!("{:<id_width$}  {:<label_width$}  {}", i.id, i.label, i.enrolled_at_ms);
	}
	Ok(())
}

/// Revoke an enrolled IDE by id (Phase 14.0). Mirror of `run_revoke`.
fn run_revoke_ide(id: &str) -> anyhow::Result<()> {
	let store = enrollment::IdeStore::open()?;
	if store.revoke(id)? {
		println!("Revoked IDE {id}");
	} else {
		println!("No IDE with id {id}");
	}
	Ok(())
}

/// Arguments for `run_serve`, mirroring the `Serve` CLI variant so the
/// clap surface and the runner stay in one-to-one shape.
struct ServeArgs {
	bind: Option<SocketAddr>,
	advertise_host: Option<String>,
	no_enrollment: bool,
	web_root: Option<std::path::PathBuf>,
	advertise_url: Option<String>,
	no_idle_exit: bool,
}

async fn run_serve(args: ServeArgs) -> anyhow::Result<()> {
	let ServeArgs {
		bind,
		advertise_host,
		no_enrollment,
		web_root,
		advertise_url,
		no_idle_exit,
	} = args;
	// Install the ring crypto provider as the process default before
	// any rustls config is built. moon-bridge's tree pulls only ring,
	// but rustls resolves its provider from a process-global slot, so
	// installing explicitly avoids a "no process-level default" panic
	// if a future dep ever drags in a second provider.
	let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();

	let bind = bind.unwrap_or_else(|| SocketAddr::from(([0, 0, 0, 0], DEFAULT_PORT)));
	let detected_ip = detect_lan_ipv4();
	// The payload's `url` uses the raw IP because it always works; the
	// `.local` name is offered alongside (`mdns_url`) since multicast
	// is blocked on some networks. The phone tries `.local` first and
	// falls back to the IP.
	let advertise_host = advertise_host.unwrap_or_else(|| {
		detected_ip
			.map(|ip| ip.to_string())
			.unwrap_or_else(|| "127.0.0.1".to_owned())
	});

	let workspaces_dir = discovery::resolve_workspaces_dir()?;
	let bridge_dir = tls::resolve_bridge_dir()?;
	// Cover the detected LAN IP in the cert SANs so a browser hitting
	// `https://<ip>:port` doesn't reject on a name mismatch. Stable
	// for a fixed IP; a network change regenerates once (logged).
	let tls_identity = tls::load_or_generate(&bridge_dir, detected_ip)?;
	let devices = pairing::DeviceStore::open()?;
	let ides = enrollment::IdeStore::open()?;

	let url = advertise_url.unwrap_or_else(|| format!("wss://{advertise_host}:{}", bind.port()));
	let mdns_url = detected_ip.map(|_| format!("wss://{}:{}", mdns::MDNS_HOSTNAME.trim_end_matches('.'), bind.port()));

	// Phone pairing has no startup window (Phase 14.5): codes are
	// minted on demand — by the local Companion panel over the
	// control socket, or by an enrolled IDE over its WS.

	// Enrollment session (Phase 14): issue a single-use code unless
	// `--no-enrollment`, print it for the operator to share with the
	// IDE's enroll UI. Startup-only because it bootstraps the trust
	// an on-demand path would need.
	let enrollment_session = if no_enrollment {
		None
	} else {
		let session = enrollment::EnrollmentSession::issue();
		println!(
			"Enrollment open for {} seconds. Share this code with an IDE's \"Connect to remote bridge\" UI:",
			enrollment::ENROLLMENT_CODE_TTL.as_secs()
		);
		println!();
		println!("  {}", session.code());
		println!();
		Some(session)
	};

	if let Some(root) = &web_root {
		println!("Serving companion PWA from {}", root.display());
	}
	tracing::info!(%bind, advertise_host, "starting moon-bridge serve");

	// Remove the control socket on exit. Not strictly required — a
	// dead socket refuses connect, which the IDE reads as
	// "not running" — but it keeps the bridge dir tidy.
	let cleanup_dir = bridge_dir.clone();
	let _guard = ControlSockCleanup(cleanup_dir);

	serve::serve(serve::ServeConfig {
		addr: bind,
		tls: tls_identity,
		workspaces_dir,
		bridge_dir,
		web_root,
		devices,
		mdns_url,
		advertise_ip: detected_ip,
		ides,
		enrollment: enrollment_session,
		idle_exit: !no_idle_exit,
		advertise_url: url,
	})
	.await
}

/// Removes the control socket on drop (bridge exit). A dead socket
/// would already read as "not running" via a refused connect; this
/// just keeps the bridge dir tidy.
struct ControlSockCleanup(camino::Utf8PathBuf);
impl Drop for ControlSockCleanup {
	fn drop(&mut self) {
		let _ = std::fs::remove_file(status::control_sock_path(&self.0));
	}
}

/// Best-effort first non-loopback IPv4 address: open a UDP socket
/// "to" a public address (no packets are sent by `connect` on UDP)
/// and read back the local address the OS picked. Avoids a
/// network-interface enumeration dep for a host that just wants its
/// own LAN IP.
fn detect_lan_ipv4() -> Option<std::net::Ipv4Addr> {
	let sock = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
	sock.connect("8.8.8.8:80").ok()?;
	match sock.local_addr().ok()?.ip() {
		IpAddr::V4(v4) if !v4.is_loopback() => Some(v4),
		_ => None,
	}
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
