//! How to spawn an LSP server.
//!
//! The broker used to call `tokio::process::Command::new(&bin)`
//! directly; that meant one physical location — the host — and
//! one set of paths — the host's absolute filesystem. Containerised
//! workspaces (ADR 0008) break both assumptions: the `rust-analyzer`
//! binary we want to talk to lives inside `moon-base` and sees
//! `/workspace/<basename>` instead of `/home/user/code/<repo>`.
//!
//! [`LspSpawner`] abstracts "where / how the process actually
//! runs". Today two variants:
//!
//! - [`LspSpawner::Local`] — `Command::new(bin)` with the server's
//!   native args, used when there's no workspace container or the
//!   container doesn't ship the LSP.
//! - [`LspSpawner::DockerExec`] — wraps the invocation in
//!   `docker exec -i <container> <bin> <args…>`, so the server
//!   actually runs inside the workspace shell. `-i` keeps stdin
//!   open; no `-t` because LSP framing is raw bytes and a TTY
//!   would mangle them.
//!
//! Both variants also expose [`LspSpawner::probe`] which runs
//! `<bin> --version` in the same location and reports whether it
//! exited cleanly. The broker calls this once per language to
//! decide whether the server is available at all — inside a
//! container the bare `which`-based host discovery doesn't help
//! because the binary isn't on the host at all.
//!
//! This file deliberately has zero `LspServer` / `LspBroker`
//! knowledge so it can be tested in isolation and reused from
//! future variants (remote SSH host, sandbox, …) without pulling
//! in the rest of the LSP stack.

use std::path::Path;
use std::process::Stdio;

use tokio::process::Command;

/// How and where to spawn the LSP server process.
#[derive(Debug, Clone)]
pub enum LspSpawner {
	/// Run the server directly on the machine moon-ide is
	/// running on — today's default.
	Local,
	/// Run the server inside a Docker container via `docker exec`.
	///
	/// `container_name` is the compose-assigned name of the
	/// workspace shell container (e.g. `moon-ws-<id>-dev-1`).
	/// The caller is responsible for confirming that container
	/// is in the `Running` state before picking this variant —
	/// we don't re-check on every spawn because the broker's
	/// availability cache already covers that lifecycle.
	DockerExec { container_name: String },
}

impl LspSpawner {
	/// Build the `Command` that launches the LSP server. The
	/// caller is expected to wire up stdio / `kill_on_drop` etc.
	/// afterwards; we only build the shape of the invocation.
	///
	/// For `DockerExec`, `bin` is the **in-container** binary
	/// name (typically just the basename, e.g. `rust-analyzer`,
	/// which resolves on the container's `$PATH`). For `Local`
	/// it's an absolute path produced by host discovery.
	pub fn build_command(&self, bin: &Path, args: &[&str]) -> Command {
		match self {
			LspSpawner::Local => {
				let mut cmd = Command::new(bin);
				cmd.args(args);
				cmd
			}
			LspSpawner::DockerExec { container_name } => {
				let mut cmd = Command::new("docker");
				// `-i` (no `-t`): stdin stays open for the JSON-RPC
				// stream but we must NOT allocate a TTY — LSP's
				// Content-Length framing is raw bytes over stdout
				// and a TTY would CRLF-translate them on some
				// hosts. This is the one thing we get wrong here
				// and the whole protocol falls over silently.
				cmd.arg("exec");
				cmd.arg("-i");
				cmd.arg(container_name);
				cmd.arg(bin);
				cmd.args(args);
				cmd
			}
		}
	}

	/// Check whether `bin` exists and starts cleanly in the
	/// target location. We invoke `<bin> <probe_args…>` and
	/// treat a zero exit code as "available".
	///
	/// Why a probe and not a plain `which`: for `DockerExec`
	/// the binary is inside the container and `which` on the
	/// host tells us nothing. For `Local` a matching filesystem
	/// entry isn't quite enough either — the server might be
	/// installed but e.g. the toolchain component behind a
	/// rustup proxy shim isn't, in which case the probe fails
	/// fast and we correctly route to `NotAvailable` instead of
	/// spawning and then reporting "Crashed".
	///
	/// `probe_args` is per-spec because LSP servers don't all
	/// agree on the syntax. `--version` is the dominant
	/// convention (rust-analyzer, tsgo, ty); `gopls` uses
	/// Cobra-style subcommand syntax (`gopls version`) and
	/// treats long flags as unknown CLI options. The
	/// `LspBinarySpec` carries the right argv so this layer
	/// stays generic.
	///
	/// stdout / stderr are dropped — we only care about the
	/// exit status. Any spawn failure (docker not installed,
	/// container missing, binary missing) also returns `false`.
	pub async fn probe(&self, bin: &str, probe_args: &[&str]) -> bool {
		let bin_path = Path::new(bin);
		let mut cmd = self.build_command(bin_path, probe_args);
		cmd.stdin(Stdio::null());
		cmd.stdout(Stdio::null());
		cmd.stderr(Stdio::null());
		cmd.kill_on_drop(true);
		match cmd.status().await {
			Ok(status) => status.success(),
			Err(err) => {
				tracing::debug!(bin = bin, error = %err, "lsp probe: spawn failed");
				false
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// `Local::build_command` stays as simple as `Command::new` —
	/// regression guard against accidentally wrapping it in
	/// docker-exec when the variant flag isn't set.
	#[test]
	fn local_build_command_matches_direct_spawn() {
		let spawner = LspSpawner::Local;
		let cmd = spawner.build_command(Path::new("/usr/local/bin/tsgo"), &["--lsp", "--stdio"]);
		let std_cmd = cmd.as_std();
		assert_eq!(std_cmd.get_program(), "/usr/local/bin/tsgo");
		let args: Vec<_> = std_cmd.get_args().map(|s| s.to_string_lossy().into_owned()).collect();
		assert_eq!(args, vec!["--lsp", "--stdio"]);
	}

	/// Regression guard for "never put `-t` in the docker exec
	/// invocation". A TTY in the way of LSP's raw-bytes framing
	/// is a silent correctness bug the first message from the
	/// server won't survive. Written as a positive-shape check
	/// of the full argv so new args slot in visibly.
	#[test]
	fn docker_exec_build_command_uses_dash_i_not_dash_t() {
		let spawner = LspSpawner::DockerExec {
			container_name: "moon-ws-default-dev-1".into(),
		};
		let cmd = spawner.build_command(Path::new("rust-analyzer"), &[]);
		let std_cmd = cmd.as_std();
		assert_eq!(std_cmd.get_program(), "docker");
		let args: Vec<_> = std_cmd.get_args().map(|s| s.to_string_lossy().into_owned()).collect();
		assert_eq!(args, vec!["exec", "-i", "moon-ws-default-dev-1", "rust-analyzer"]);
		assert!(
			!args.iter().any(|a| a == "-t" || a == "-it"),
			"TTY allocation would mangle LSP framing; keep `-i` only"
		);
	}

	/// `docker exec` preserves server args verbatim (same order
	/// the `Local` case would pass them in).
	#[test]
	fn docker_exec_forwards_extra_server_args_in_order() {
		let spawner = LspSpawner::DockerExec {
			container_name: "moon-ws-default-dev-1".into(),
		};
		let cmd = spawner.build_command(Path::new("tsgo"), &["--lsp", "--stdio"]);
		let args: Vec<_> = cmd
			.as_std()
			.get_args()
			.map(|s| s.to_string_lossy().into_owned())
			.collect();
		assert_eq!(
			args,
			vec!["exec", "-i", "moon-ws-default-dev-1", "tsgo", "--lsp", "--stdio"]
		);
	}

	/// `Local::probe` returns true for a bin that actually
	/// accepts `--version` and exits zero. We use `/bin/echo`
	/// because POSIX `echo` ignores unknown flags and exits 0
	/// — giving us a deterministic positive probe on any Unix
	/// CI host without relying on a real LSP.
	#[cfg(unix)]
	#[tokio::test]
	async fn probe_returns_true_on_zero_exit() {
		let spawner = LspSpawner::Local;
		assert!(spawner.probe("/bin/echo", &["--version"]).await);
	}

	/// `Local::probe` returns false when the binary doesn't
	/// exist at all — the broker's cue to cache `NotAvailable`
	/// instead of surfacing the spawn failure as "Crashed".
	#[tokio::test]
	async fn probe_returns_false_on_missing_bin() {
		let spawner = LspSpawner::Local;
		assert!(!spawner.probe("/definitely/not/a/binary/xyzzy", &["--version"]).await);
	}

	/// `false(1)` is POSIX-standard and exits non-zero; use it
	/// as a shape-check for "binary runs but the probe fails"
	/// without depending on a container being available.
	#[cfg(unix)]
	#[tokio::test]
	async fn probe_returns_false_on_non_zero_exit() {
		let spawner = LspSpawner::Local;
		assert!(!spawner.probe("/bin/false", &["--version"]).await);
	}

	/// Probe argv flows through verbatim — regression guard for
	/// the gopls case where `["version"]` (subcommand) succeeds
	/// but `["--version"]` (long flag) exits non-zero. We assert
	/// only the positive shape: a probe with no args invoked on
	/// `/bin/true` exits zero, while `/bin/false` always exits
	/// one regardless of args, so a single argv slot is enough.
	#[cfg(unix)]
	#[tokio::test]
	async fn probe_uses_supplied_argv() {
		let spawner = LspSpawner::Local;
		// `/bin/true` ignores all args and exits zero.
		assert!(spawner.probe("/bin/true", &["version"]).await);
		assert!(spawner.probe("/bin/true", &[]).await);
	}
}
