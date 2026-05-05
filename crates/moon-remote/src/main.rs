//! moon-remote — the remote-host counterpart to the moon-ide host process.
//!
//! Phase 0 ships a stub. The future `RemoteHost` story (SSH / Codespaces,
//! where the host and the workspace don't share a filesystem) turns this
//! into a JSON-RPC server over a Unix socket exposing `moon-core`'s
//! `WorkspaceHost`.
//!
//! Phase 2 (local containers) does **not** use this binary — local
//! containers use bind-mount + `docker exec` instead. See
//! [specs/containers.md](../../../specs/containers.md) and
//! [specs/architecture.md](../../../specs/architecture.md).
//!
//! Renamed from `moon-agent` per
//! [ADR 0011](../../../specs/decisions/0011-rename-moon-agent-to-moon-remote.md):
//! the word "agent" is now reserved for the in-process AI coding
//! agent (`moon-coder`).

use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "moon-remote", version, about = "moon-ide in-container remote runtime")]
struct Args {
	/// Listen address (future remote variant): `unix:///path/to/sock` or `tcp://host:port`.
	#[arg(long, default_value = "unix:///tmp/moon-remote.sock")]
	listen: String,
}

fn main() -> anyhow::Result<()> {
	tracing_subscriber::fmt()
		.with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
		.init();

	let args = Args::parse();

	tracing::info!(
		protocol_version = moon_protocol::PROTOCOL_VERSION,
		listen = %args.listen,
		"moon-remote starting (JSON-RPC server lands with the future remote host)"
	);

	Err(anyhow::anyhow!(
		"moon-remote JSON-RPC server is not implemented yet (future remote host)"
	))
}
