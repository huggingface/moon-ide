//! Workspace discovery for the mobile companion bridge.
//!
//! The bridge's job (eventually) is to relay the coder + git
//! surface of a running moon-ide workspace process to a phone over
//! the LAN. Step zero — this module — is figuring out *which*
//! workspaces are running right now.
//!
//! Per [ADR 0014](../../../specs/decisions/0014-process-per-workspace.md),
//! moon-ide runs one OS process per workspace, and each owning
//! process binds a Unix domain socket at
//! `<workspaces_dir>/<slug>/instance.sock` for single-instance
//! enforcement + focus IPC. The set of *live* sockets is therefore
//! an authoritative registry of "what's open": a socket that
//! accepts a connection has a running owner; one that refuses
//! (`ECONNREFUSED`) or is missing is a stale file from a crash.
//!
//! This is the same liveness probe `src-tauri`'s focus-socket code
//! already uses (`workspace_is_live` / `probe_alive`); we
//! re-implement the few lines here rather than depend on
//! `src-tauri` (a binary crate, not a library) so the bridge stays
//! a leaf that links only `moon-core` + `moon-protocol`.
//!
//! See [`specs/companion.md`](../../../specs/companion.md) for the
//! whole-system design and
//! [`specs/roadmaps/phase-13-mobile-companion.md`](../../../specs/roadmaps/phase-13-mobile-companion.md)
//! § 13.0 for this sub-phase's acceptance.

use std::time::Duration;

use camino::{Utf8Path, Utf8PathBuf};
use moon_protocol::workspace::WorkspaceMeta;
use tokio::net::UnixStream;

/// Bundle identifier — the directory segment moon-ide uses under
/// both the config dir (`state.json` catalog) and the local data
/// dir (per-workspace state). Must match `BUNDLE_IDENTIFIER` in
/// `src-tauri/src/lib.rs`; there's no shared constant to import
/// because that one lives in the binary crate.
const BUNDLE_IDENTIFIER: &str = "moon-ide";

/// How long we wait for a socket `connect()` before deciding the
/// owner is gone. Matches the 250 ms `CONNECT_TIMEOUT` the
/// focus-socket prober uses — generous on a healthy local socket,
/// short enough that scanning a directory full of stale files
/// stays snappy.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(250);

/// A workspace the bridge found on disk and probed for liveness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredWorkspace {
	/// Workspace slug — the on-disk directory name, which is also
	/// the `moon-ide --workspace <id>` argument.
	pub id: String,
	/// Human-readable label from the catalog, if the workspace has
	/// an entry there. Falls back to the slug when the catalog has
	/// no matching row (e.g. a state dir left behind after the
	/// catalog entry was deleted).
	pub name: String,
	/// Last-active timestamp (Unix epoch seconds) from the catalog,
	/// or `None` when the workspace has no catalog entry.
	pub last_active_at: Option<i64>,
	/// Whether a process is currently listening on the workspace's
	/// `instance.sock`. `false` means the directory exists but no
	/// owner is running (stale socket or simply not open).
	pub live: bool,
}

/// Resolve the directory holding per-workspace state dirs:
/// `<data_local_dir>/moon-ide/workspaces`. Mirrors
/// `resolve_workspaces_dir` in `src-tauri/src/lib.rs`.
pub fn resolve_workspaces_dir() -> anyhow::Result<Utf8PathBuf> {
	let raw = dirs::data_local_dir()
		.ok_or_else(|| anyhow::anyhow!("could not resolve local data dir for the current platform"))?;
	let utf8 =
		Utf8PathBuf::from_path_buf(raw).map_err(|p| anyhow::anyhow!("non-utf8 local data dir: {}", p.display()))?;
	Ok(utf8.join(BUNDLE_IDENTIFIER).join("workspaces"))
}

/// Resolve the config dir holding `state.json` (the workspace
/// catalog): `<config_dir>/moon-ide`. Mirrors how `src-tauri`
/// builds the config dir it hands `moon_core::app_state::load`.
pub fn resolve_config_dir() -> anyhow::Result<Utf8PathBuf> {
	let raw =
		dirs::config_dir().ok_or_else(|| anyhow::anyhow!("could not resolve config dir for the current platform"))?;
	let utf8 = Utf8PathBuf::from_path_buf(raw).map_err(|p| anyhow::anyhow!("non-utf8 config dir: {}", p.display()))?;
	Ok(utf8.join(BUNDLE_IDENTIFIER))
}

/// Path of a workspace's instance socket. Mirrors
/// `focus_socket::socket_path` in `src-tauri`. The socket lives in
/// a dedicated `run/` subdirectory so the dev container can mount
/// that directory rather than the socket file (ADR 0026).
pub fn socket_path(workspaces_dir: &Utf8Path, slug: &str) -> Utf8PathBuf {
	workspaces_dir.join(slug).join("run").join("instance.sock")
}

/// Connect-and-handshake liveness check. We can't trust
/// `Path::exists` because a crashed process leaves the socket file
/// behind — only a real listener accepts the connection.
async fn probe_alive(path: &Utf8Path) -> bool {
	if !path.exists() {
		return false;
	}
	let connect = tokio::time::timeout(CONNECT_TIMEOUT, UnixStream::connect(path.as_std_path())).await;
	matches!(connect, Ok(Ok(_)))
}

/// Read the workspace catalog (`state.json`) into a slug → meta
/// map. A missing or unparseable catalog yields an empty map —
/// `moon_core::app_state::load` already degrades to defaults on
/// parse failure, so discovery never fails on a corrupt catalog;
/// it just loses the human-readable names and falls back to slugs.
async fn load_catalog(config_dir: &Utf8Path) -> Vec<WorkspaceMeta> {
	match moon_core::app_state::load(config_dir).await {
		Ok(state) => state.workspaces,
		Err(err) => {
			tracing::warn!(error = %err, "failed to load workspace catalog; discovery will use slugs as names");
			Vec::new()
		}
	}
}

/// Enumerate every workspace state dir under `workspaces_dir`,
/// probe each for a live owner, and decorate the result with
/// catalog metadata (name + last-active) when available.
///
/// Returns workspaces sorted by liveness (live first), then by
/// `last_active_at` descending (most-recent first), then by slug —
/// so the natural "pick one" order on the phone shows running,
/// recently-touched workspaces at the top. A missing
/// `workspaces_dir` (fresh install, nobody's opened a workspace
/// yet) yields an empty list, not an error.
pub async fn discover(workspaces_dir: &Utf8Path, config_dir: &Utf8Path) -> anyhow::Result<Vec<DiscoveredWorkspace>> {
	let catalog = load_catalog(config_dir).await;

	let mut read_dir = match tokio::fs::read_dir(workspaces_dir.as_std_path()).await {
		Ok(rd) => rd,
		Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
		Err(err) => return Err(anyhow::Error::from(err).context(format!("reading {workspaces_dir}"))),
	};

	let mut out = Vec::new();
	while let Some(entry) = read_dir.next_entry().await? {
		let file_type = entry.file_type().await?;
		if !file_type.is_dir() {
			continue;
		}
		let Some(slug) = entry.file_name().to_str().map(str::to_owned) else {
			tracing::warn!("skipping non-utf8 workspace dir name");
			continue;
		};

		let sock = socket_path(workspaces_dir, &slug);
		let live = probe_alive(&sock).await;

		let meta = catalog.iter().find(|m| m.id == slug);
		let name = meta.map(|m| m.name.clone()).unwrap_or_else(|| slug.clone());
		let last_active_at = meta.map(|m| m.last_active_at);

		out.push(DiscoveredWorkspace {
			id: slug,
			name,
			last_active_at,
			live,
		});
	}

	out.sort_by(|a, b| {
		b.live
			.cmp(&a.live)
			.then_with(|| b.last_active_at.cmp(&a.last_active_at))
			.then_with(|| a.id.cmp(&b.id))
	});

	Ok(out)
}

#[cfg(test)]
mod tests {
	use super::*;

	/// A workspace dir with no live listener is reported `live: false`
	/// and falls back to the slug for its name when the catalog is
	/// empty.
	#[tokio::test]
	async fn discovers_dirs_and_reports_dead_sockets() {
		let tmp = tempdir();
		let workspaces_dir = Utf8PathBuf::from_path_buf(tmp.path().join("workspaces")).unwrap();
		let config_dir = Utf8PathBuf::from_path_buf(tmp.path().join("config")).unwrap();
		std::fs::create_dir_all(workspaces_dir.join("huggingface")).unwrap();
		std::fs::create_dir_all(workspaces_dir.join("gitaly").join("run")).unwrap();
		// A stale socket file with nobody listening must read as dead.
		std::fs::write(socket_path(&workspaces_dir, "gitaly"), b"").unwrap();

		let found = discover(&workspaces_dir, &config_dir).await.unwrap();
		assert_eq!(found.len(), 2);
		assert!(found.iter().all(|w| !w.live));
		let hf = found.iter().find(|w| w.id == "huggingface").unwrap();
		assert_eq!(hf.name, "huggingface");
		assert_eq!(hf.last_active_at, None);
	}

	/// A live socket (we bind one ourselves to stand in for a running
	/// IDE process) is reported `live: true`.
	#[tokio::test]
	async fn reports_live_socket_as_live() {
		let tmp = tempdir();
		let workspaces_dir = Utf8PathBuf::from_path_buf(tmp.path().join("workspaces")).unwrap();
		let config_dir = Utf8PathBuf::from_path_buf(tmp.path().join("config")).unwrap();
		let sock = socket_path(&workspaces_dir, "alive");
		std::fs::create_dir_all(sock.parent().unwrap()).unwrap();
		let _listener = tokio::net::UnixListener::bind(sock.as_std_path()).unwrap();

		let found = discover(&workspaces_dir, &config_dir).await.unwrap();
		let alive = found.iter().find(|w| w.id == "alive").unwrap();
		assert!(alive.live);
	}

	/// A missing workspaces dir is an empty result, not an error.
	#[tokio::test]
	async fn missing_dir_is_empty_not_error() {
		let tmp = tempdir();
		let workspaces_dir = Utf8PathBuf::from_path_buf(tmp.path().join("does-not-exist")).unwrap();
		let config_dir = Utf8PathBuf::from_path_buf(tmp.path().join("config")).unwrap();
		let found = discover(&workspaces_dir, &config_dir).await.unwrap();
		assert!(found.is_empty());
	}

	/// Files (not directories) under the workspaces dir are ignored.
	#[tokio::test]
	async fn ignores_non_directory_entries() {
		let tmp = tempdir();
		let workspaces_dir = Utf8PathBuf::from_path_buf(tmp.path().join("workspaces")).unwrap();
		let config_dir = Utf8PathBuf::from_path_buf(tmp.path().join("config")).unwrap();
		std::fs::create_dir_all(&workspaces_dir).unwrap();
		std::fs::write(workspaces_dir.join("stray.txt"), b"junk").unwrap();
		std::fs::create_dir_all(workspaces_dir.join("real")).unwrap();

		let found = discover(&workspaces_dir, &config_dir).await.unwrap();
		assert_eq!(found.len(), 1);
		assert_eq!(found[0].id, "real");
	}

	/// Minimal temp dir helper — avoids pulling `tempfile` into the
	/// crate for four tests; the OS temp dir + a random suffix is
	/// enough, and we clean up on drop.
	struct TmpDir(std::path::PathBuf);
	impl TmpDir {
		fn path(&self) -> &std::path::Path {
			&self.0
		}
	}
	impl Drop for TmpDir {
		fn drop(&mut self) {
			let _ = std::fs::remove_dir_all(&self.0);
		}
	}
	fn tempdir() -> TmpDir {
		let nanos = std::time::SystemTime::now()
			.duration_since(std::time::UNIX_EPOCH)
			.unwrap()
			.as_nanos();
		let p = std::env::temp_dir().join(format!("moon-bridge-test-{nanos}-{:?}", std::thread::current().id()));
		std::fs::create_dir_all(&p).unwrap();
		TmpDir(p)
	}
}
