//! moon-agent — the in-container counterpart to the moon-ide host process.
//!
//! Phase 0 ships a stub. Phase 2 turns this into a JSON-RPC server over a
//! Unix socket exposing `moon-core`'s `WorkspaceHost`.
//!
//! See [specs/devcontainers.md](../../../specs/devcontainers.md).

use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "moon-agent", version, about = "moon-ide in-container agent")]
struct Args {
	/// Listen address. Phase 2: `unix:///path/to/sock` or `tcp://host:port`.
	#[arg(long, default_value = "unix:///tmp/moon-agent.sock")]
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
		"moon-agent starting (Phase 2 will wire JSON-RPC server here)"
	);

	Err(anyhow::anyhow!(
		"moon-agent JSON-RPC server is not implemented yet (Phase 2)"
	))
}
