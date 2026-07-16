//! Per-language LSP server actor.
//!
//! One [`LspServer`] owns one child process (e.g. `tsgo --lsp --stdio`),
//! an [`LspClient`] on top of its stdio, and a map of open documents.
//! Its public surface is narrow: `open` / `update` / `close` /
//! `hover` / `completion`. Nothing here knows about multiple
//! languages — the broker picks the right server for a language id.
//!
//! Lifecycle:
//! 1. `LspServer::spawn` locates the binary, starts the child,
//!    builds the client, sends `initialize` + `initialized`.
//! 2. The broker calls `open` / `update` / `close` as the user
//!    interacts with buffers. We send full-document sync
//!    (`TextDocumentSyncKind::FULL`) for now — correctness over
//!    throughput while the client surface is tiny.
//! 3. `publishDiagnostics` notifications are forwarded out through
//!    the broker's event channel (translated to
//!    `moon_protocol::lsp` shapes).
//! 4. Dropping the server sends `shutdown` + `exit` so the child
//!    can flush, then aborts if it hangs.
//!
//! stderr of the child process is piped to `tracing::debug` — LSP
//! servers are chatty on stderr about things we'd rather not see in
//! the user log at INFO.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use camino::{Utf8Path, Utf8PathBuf};
use lsp_types as lt;
use moon_protocol::lsp as mp;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Child;
use tokio::sync::{broadcast, Mutex};

use super::client::{LspClient, LspClientError, ServerNotification};
use super::spawn::LspSpawner;
use super::translate;

/// What the broker hears when something interesting happens in a
/// server — feeds through to the Tauri event layer.
#[derive(Debug, Clone)]
pub enum LspServerEvent {
	Diagnostics(mp::LspDiagnosticsEvent),
	StatusChanged(mp::LspStatusEvent),
}

/// Which binary to spawn for a given LSP language. One entry per
/// language id we intend to support.
///
/// `install_hint` is what the status bar's "not available" pill
/// suggests — short and actionable, no prose. On the happy path the
/// hint is never shown; it only surfaces when discovery failed.
pub struct LspBinarySpec {
	pub language_id: &'static str,
	pub bin_name: &'static str,
	pub args: &'static [&'static str],
	/// Argv used by the broker's availability probe (`<bin>
	/// <probe_args…>` — exit zero means "available"). Almost
	/// every LSP accepts `--version`; the odd exception is
	/// `gopls`, which uses subcommand syntax (`gopls version`)
	/// and treats the long flag as an unknown CLI option. Per-
	/// spec override keeps the probe generic without forcing
	/// every server to follow the same convention.
	pub probe_args: &'static [&'static str],
	pub install_hint: &'static str,
	pub discovery: DiscoveryStrategy,
}

/// Where to look for `bin_name` before falling through to `$PATH`.
///
/// Each strategy picks one ecosystem-idiomatic location: Node users
/// expect `node_modules/.bin/` to win even when there's a global
/// install, Rust users expect `~/.cargo/bin/` to be found even when
/// the Tauri process didn't inherit the shell's `PATH` (which
/// happens on GUI-launched apps on macOS / some Linux DEs).
#[derive(Clone, Copy)]
pub enum DiscoveryStrategy {
	/// Walk ancestors from the workspace root looking for
	/// `node_modules/.bin/<bin>`, then `$PATH`. Matches Node's own
	/// resolution so pnpm-hoisted monorepos work without special
	/// casing.
	NodeModules,
	/// Check `$CARGO_HOME/bin/<bin>` (falling back to
	/// `$HOME/.cargo/bin/<bin>`), then `$PATH`. Covers
	/// `rustup component add rust-analyzer` whose default install
	/// location isn't always on the launched Tauri process's
	/// inherited `PATH`.
	CargoHome,
	/// Walk ancestors from the workspace root looking for
	/// `.venv/bin/<bin>` (Unix) / `.venv/Scripts/<bin>.exe`
	/// (Windows), then `$PATH`. Mirrors `NodeModules` for the
	/// Python ecosystem: `.venv/` is `uv`'s default virtualenv
	/// layout (and the shape every modern Python project lands
	/// on), so a `uv pip install ty` / `uv add --dev ty` lands
	/// where we look first. The PATH fallback catches users who
	/// did `uv tool install ty` instead and have `~/.local/bin`
	/// on PATH.
	PythonVenv,
	/// Check `$GOBIN/<bin>`, then `$GOPATH/bin/<bin>` (with
	/// `$GOPATH` defaulting to `$HOME/go`), then `$PATH`. Covers
	/// the canonical `go install golang.org/x/tools/gopls@latest`
	/// install path — Go has no per-project install convention
	/// (binaries always land in the user-wide GOPATH), so the
	/// shape mirrors `CargoHome` rather than `NodeModules`.
	GoBin,
}

/// TypeScript / JavaScript server.
///
/// We target `tsgo` (Microsoft's native Go port of TypeScript, shipped
/// as `@typescript/native-preview`) rather than the community
/// `typescript-language-server` wrapper. TS 7's `typescript` package
/// also ships the same native binary renamed to `tsc` (speaking the
/// same `--lsp --stdio`), but that package drops the programmatic JS
/// API — so tooling that embeds the compiler (`svelte2tsx`, used by
/// `svelte-fast-check`) still needs the classic `typescript@6` for
/// its API, which is why `tsgo` stays the *preferred* binary. A
/// project that ships only `typescript@7` still gets LSP: discovery
/// falls back to the project's `.bin/tsc` when the resolved
/// `typescript` package is major ≥ 7 (see [`discover_server_binary`]
/// — the version gate matters because typescript@6's `tsc` is the JS
/// compiler with no `--lsp` mode). If a project ships
/// `typescript-language-server` instead, flip this spec — the LSP
/// wire format is identical and nothing else has to change. See
/// [`specs/lsp.md`].
pub const TS_SERVER: LspBinarySpec = LspBinarySpec {
	language_id: "typescript",
	bin_name: "tsgo",
	args: &["--lsp", "--stdio"],
	probe_args: &["--version"],
	install_hint: "bun add -D @typescript/native-preview",
	discovery: DiscoveryStrategy::NodeModules,
};

/// Resolve the install hint to surface for `spec` in the context of
/// the workspace `root`. Most specs return their static
/// `install_hint` verbatim — there's only one canonical install path
/// for `rust-analyzer`, `gopls`, `ty`. The TS server is the
/// exception: a developer using pnpm or npm shouldn't be told to run
/// `bun add` — that would either fail (no `bun.lock`, package
/// manager mismatch) or quietly create a parallel `bun.lock`
/// alongside the existing lockfile, both bad. Picking the command
/// from the root lockfile keeps the pill tooltip copy-pasteable for
/// whichever package manager the project actually uses.
///
/// Detection is strictly file-presence at the workspace root:
/// `pnpm-lock.yaml` → `pnpm -wD add` (the `-w` flag points pnpm at
/// the workspace root, which is the typical shape for monorepos
/// using pnpm); `package-lock.json` → `npm i -D`; otherwise (`bun.lock`
/// or no recognised lockfile) → the static `bun add -D` hint, which
/// matches moon-ide itself and is a reasonable default for a fresh
/// repo.
pub fn resolve_install_hint(spec: &LspBinarySpec, root: &Utf8Path) -> String {
	// npm-distributed servers get the lockfile-aware treatment; the
	// static hint (a `bun add`) is the fallback for `bun.lock` or no
	// recognised lockfile.
	let npm_package = match spec.language_id {
		"typescript" => Some("@typescript/native-preview"),
		"oxlint" => Some("oxlint"),
		"svelte" => Some("svelte-language-server"),
		_ => None,
	};
	let Some(pkg) = npm_package else {
		return spec.install_hint.to_string();
	};
	if root.join("pnpm-lock.yaml").exists() {
		return format!("pnpm -wD add {pkg}");
	}
	if root.join("package-lock.json").exists() {
		return format!("npm i -D {pkg}");
	}
	spec.install_hint.to_string()
}

/// Rust server — `rust-analyzer`, the ecosystem-standard LSP.
///
/// No per-project install exists for Rust LSPs (unlike `tsgo`), so we
/// rely on the system toolchain: `rustup component add rust-analyzer`
/// drops it at `$CARGO_HOME/bin/rust-analyzer`, which is where we
/// look first. A `cargo install rust-analyzer` build lands in the
/// same place. `$PATH` is the last resort for hand-compiled or
/// package-manager-installed copies.
///
/// No args: `rust-analyzer` defaults to stdio + LSP, which is
/// exactly the contract we want. The binary auto-detects the
/// workspace layout from `initialize.workspaceFolders`, which the
/// generic `initialize` below already sends.
pub const RUST_SERVER: LspBinarySpec = LspBinarySpec {
	language_id: "rust",
	bin_name: "rust-analyzer",
	args: &[],
	probe_args: &["--version"],
	install_hint: "rustup component add rust-analyzer",
	discovery: DiscoveryStrategy::CargoHome,
};

/// Python server — `ty`, Astral's native type checker + language
/// server. Same vendor as `uv` and `ruff`, ships as a single
/// statically-linked Rust binary, advertises an LSP under the
/// `ty server` subcommand (matching `ruff server`'s convention).
///
/// Discovery prefers a project-local `.venv/bin/ty` (where
/// `uv pip install ty` / `uv add --dev ty` lands) over a global
/// install — same shape as `tsgo` / `node_modules/.bin/`, so a
/// project that pins a specific `ty` release isn't shadowed by
/// a different one on the user's `$PATH`. `uv tool install ty`
/// (which writes to `~/.local/bin/ty`) is picked up via the
/// PATH fallback.
///
/// `ty` is in beta as of 2026 — if a feature gap blocks us,
/// switching to `pyright-langserver` / `pylsp` is a one-string
/// edit on this spec, the rest of the broker is wire-agnostic.
pub const PYTHON_SERVER: LspBinarySpec = LspBinarySpec {
	language_id: "python",
	bin_name: "ty",
	args: &["server"],
	probe_args: &["--version"],
	install_hint: "uv add --dev ty (or uv tool install ty)",
	discovery: DiscoveryStrategy::PythonVenv,
};

/// Go server — `gopls`, the official LSP from the Go team
/// (`golang.org/x/tools/gopls`). Same posture as `rust-analyzer`:
/// no per-project install convention, the binary always lives in
/// the user-wide GOPATH after `go install`. Discovery prefers
/// `$GOBIN/gopls`, then `$GOPATH/bin/gopls` (with `$GOPATH`
/// defaulting to `$HOME/go` per the Go toolchain's own default),
/// then `$PATH`.
///
/// No startup args: `gopls` defaults to stdio + LSP when invoked
/// with no flags. It auto-detects the workspace layout from
/// `initialize.workspaceFolders` and reads `go.mod` / `go.work`
/// itself.
///
/// `probe_args` is `["version"]`, not `["--version"]`: gopls is
/// the one server in our roster that uses Cobra-style subcommand
/// syntax instead of long flags, and treats `--version` as an
/// unknown flag (exit 2). The probe would otherwise cache
/// `NotAvailable` even when the binary is happily installed.
pub const GO_SERVER: LspBinarySpec = LspBinarySpec {
	language_id: "go",
	bin_name: "gopls",
	args: &[],
	probe_args: &["version"],
	install_hint: "go install golang.org/x/tools/gopls@latest",
	discovery: DiscoveryStrategy::GoBin,
};

/// Svelte server — `svelteserver` from the `svelte-language-server`
/// package, the same server the official VS Code extension embeds.
/// Covers the whole component surface: TS/JS in `<script>` (via its
/// own `svelte2tsx` projection), CSS in `<style>`, and the template.
///
/// Discovery is `NodeModules`, same as `tsgo` / `oxlint`: the
/// per-project install is first-class (`bun add -D
/// svelte-language-server`) and the package declares a `typescript`
/// peer (^5.9 || ^6) the project supplies — two reasons it is *not*
/// baked into `moon-base`. The `.bin` shim is a Node script
/// (shebang-resolved), which is a given for any project that has
/// `.svelte` files and a `node_modules/` in the first place.
///
/// `probe_args` is empty: the bin script has no `--version` flag —
/// it unconditionally starts the LSP loop. The probe still
/// terminates deterministically because [`LspSpawner::probe`] nulls
/// stdin: the connection sees EOF immediately and the process exits
/// zero. Costs one Node startup, paid once per routing decision.
pub const SVELTE_SERVER: LspBinarySpec = LspBinarySpec {
	language_id: "svelte",
	bin_name: "svelteserver",
	args: &["--stdio"],
	probe_args: &[],
	install_hint: "bun add -D svelte-language-server",
	discovery: DiscoveryStrategy::NodeModules,
};

/// JS/TS linter — `oxlint`, run as a language server via its
/// built-in `--lsp` flag.
///
/// **Co-tenant with the language server.** This is *not* a
/// replacement for `tsgo` — it runs alongside on the same files,
/// publishing its own `publishDiagnostics` reports stamped with
/// `producer: "oxlint"`. The frontend keys diagnostics by
/// `(path, producer)` so type errors from `tsgo` and lint
/// warnings from `oxlint` coexist on the same line.
///
/// `oxlint --lsp` was wired up in oxc 1.47 (see oxc-project/oxc
/// PRs #19292, #20321) and speaks standard LSP framing over
/// stdio. Discovery prefers the project-local `node_modules/.bin/oxlint`
/// — same `NodeModules` walk we use for `tsgo` — so a project
/// pinning a specific oxlint version isn't shadowed by a global
/// install.
///
/// `language_id: "oxlint"` is the broker slot key (the producer
/// stamp on diagnostics, the status-bar pill name). The
/// per-document `textDocument.languageId` carried in `didOpen`
/// is still the file's real language (`"typescript"`,
/// `"javascriptreact"`, …) — that's what oxlint needs to know
/// which parser to use.
pub const OXLINT_LINTER: LspBinarySpec = LspBinarySpec {
	language_id: "oxlint",
	bin_name: "oxlint",
	args: &["--lsp"],
	probe_args: &["--version"],
	install_hint: "bun add -D oxlint",
	discovery: DiscoveryStrategy::NodeModules,
};

/// File language ids that the linter co-tenant covers. Used by
/// the broker to decide whether to spawn / route to oxlint for a
/// given `lsp_open` / `lsp_update` call. Mirrors `tsgo`'s set.
pub const OXLINT_LANGUAGES: &[&str] = &["typescript", "typescriptreact", "javascript", "javascriptreact"];

/// Maximum walk depth when scanning for nested `.oxlintrc.json`
/// files. `apps/<pkg>/`, `packages/<pkg>/`, `services/<pkg>/foo/`
/// — five is enough to cover every monorepo layout we care about
/// while keeping the scan cost on a fresh `.gitignore` walk well
/// under a second. `WalkBuilder` already prunes `node_modules/`,
/// `.git/`, and ignored directories so the depth budget isn't
/// spent on vendored files.
const OXLINT_CONFIG_SCAN_DEPTH: usize = 5;

/// Discover host-relative directories that contain a
/// `.oxlintrc.json`, anchored at `host_root`. The empty string
/// (`""`) is always present and stands for the root itself; nested
/// hits are returned as forward-slash paths relative to the root,
/// sorted for deterministic output. The result is what the broker
/// hands to [`LspServer::spawn`] as `workspace_folders` so oxlint
/// anchors its per-folder config discovery on each containing
/// package — a `.oxlintrc.json` in `apps/api/` is invisible to the
/// LSP unless `apps/api` is advertised as its own workspace folder.
///
/// `.gitignore`-aware via `ignore::WalkBuilder`, so a
/// `node_modules/oxlint/` dropping a sample `.oxlintrc.json`
/// doesn't get advertised. Bounded by [`OXLINT_CONFIG_SCAN_DEPTH`]
/// for the same reason the binary discovery is bounded — the
/// walk needs to be cheap enough to run on every broker spawn.
pub fn discover_oxlint_workspace_folders(host_root: &Path) -> Vec<String> {
	use ignore::WalkBuilder;
	let mut out: Vec<String> = vec![String::new()];
	let walker = WalkBuilder::new(host_root)
		.hidden(false)
		.git_ignore(true)
		.git_exclude(true)
		.ignore(true)
		.max_depth(Some(OXLINT_CONFIG_SCAN_DEPTH))
		.build();
	for entry in walker.flatten() {
		if entry.depth() == 0 {
			continue;
		}
		let path = entry.path();
		if !path.is_file() {
			continue;
		}
		if path.file_name().and_then(|s| s.to_str()) != Some(".oxlintrc.json") {
			continue;
		}
		let Some(parent) = path.parent() else {
			continue;
		};
		if parent == host_root {
			// Root config — already covered by the empty entry.
			continue;
		}
		let Ok(rel) = parent.strip_prefix(host_root) else {
			continue;
		};
		let Some(rel_str) = rel.to_str() else {
			continue;
		};
		// Forward slashes on the wire for cross-platform parity
		// with the rest of the LSP path plumbing (which already
		// normalises this way in `PathTranslator::relativise`).
		let normalised = rel_str.replace('\\', "/");
		if !normalised.is_empty() {
			out.push(normalised);
		}
	}
	out.sort();
	out.dedup();
	out
}

pub struct LspServer {
	language_id: String,
	client: LspClient,
	child: Mutex<Option<Child>>,
	/// Maps between host-relative paths and server-side file URIs.
	/// `Identity` for [`LspSpawner::Local`], `HostMount` when the
	/// server runs inside a container and sees `/workspace/<basename>`
	/// instead of the host absolute path. Replaces the old bare
	/// `root` field; all URI construction + parsing routes through
	/// here so the wire format matches what the server expects.
	translator: PathTranslator,
	/// Workspace folders this server was spawned with — host-side
	/// **relative** to `translator.host_root()`, with `""` for the
	/// root itself. Sent in `initialize` (translated through
	/// [`PathTranslator::absolutise`] so container-routed servers
	/// see `/workspace/<basename>/<rel>` URIs). Multi-root matters
	/// for linter co-tenants: oxlint anchors its config discovery
	/// per workspace folder, not per file, so a monorepo with a
	/// `.oxlintrc.json` in each package has to advertise each
	/// containing directory as its own folder or the per-package
	/// rule overrides silently never take effect. Single-entry
	/// (just the root) for language servers — they don't gain
	/// anything from a wider list and tsgo / rust-analyzer index
	/// the whole tree from rootUri regardless.
	workspace_folders: Vec<String>,
	// Per-document version counter. LSP requires monotonically
	// increasing versions per didChange to detect out-of-order
	// updates; we start at 1 on open and tick each change.
	docs: Mutex<HashMap<String, DocState>>,
	/// Glob patterns the server registered for
	/// `workspace/didChangeWatchedFiles` notifications, keyed on
	/// the registration id so an `unregisterCapability` can drop
	/// just that entry. Populated lazily by the notification pump
	/// when the server fires `client/registerCapability`; empty
	/// for servers that don't ask for fs-event notifications
	/// (which makes [`notify_files_changed`] a no-op for them —
	/// safe and free).
	watched_patterns: Mutex<HashMap<String, Vec<WatchedPattern>>>,
	/// Snapshot of `result.capabilities.completion_provider.resolve_provider`
	/// from the server's `initialize` response. Tells us whether
	/// `completionItem/resolve` is meaningful for this server —
	/// the LSP auto-import pipeline (`tsgo`, `rust-analyzer`,
	/// `pyright`) lazy-resolves `additionalTextEdits` and gates
	/// the import line on this flag. When `false`, the broker
	/// short-circuits resolve and returns whatever the initial
	/// `textDocument/completion` projection already carried
	/// instead of round-tripping a no-op call. `AtomicBool` so
	/// reads from the request handlers don't take the docs lock;
	/// stored once at `initialize` time, never written again
	/// during the server's lifetime.
	completion_resolve_provider: std::sync::atomic::AtomicBool,
	/// Fan-out sink the server uses to publish its own events
	/// (diagnostics, status changes). Cloned from the broker's
	/// channel so the pull-diagnostics task can shove `LspDiagnostics`
	/// events directly through the same surface as the push pump.
	events: broadcast::Sender<LspServerEvent>,
}

/// One compiled glob the server cares about, plus the
/// LSP-flavoured event-type bitmask. Default kind per spec is
/// 7 (Create | Change | Delete) when the server omits it.
struct WatchedPattern {
	matcher: globset::GlobMatcher,
	kind: lt::WatchKind,
}

/// Compile every glob from a `workspace/didChangeWatchedFiles`
/// registration into the in-memory matcher list. Free function
/// (rather than a method) so the parsing logic can be unit-
/// tested without spinning up a real LSP server child process.
///
/// `Relative` glob patterns are flattened to their `pattern`
/// string and matched against workspace-relative paths. The
/// spec scopes them to a `WorkspaceFolder`, but moon-ide opens
/// one folder per broker so the relative form would point at
/// the same root either way. Servers that ship only string
/// patterns (the common case for tsgo / rust-analyzer / gopls)
/// hit the simpler arm.
///
/// Returns `None` when the registration carries no usable
/// patterns — empty `watchers` list, all globs failed to
/// compile (logged at warn), or `register_options` was missing.
/// The caller treats that the same as "we never saw the
/// registration", which is the right behaviour: the spec's
/// auto-`null` reply already told the server we accepted, but
/// not knowing how to compile any of its patterns means we
/// silently won't fire watched-files notifications, identical
/// to the pre-B+C state. No correctness regression.
fn parse_watched_files_registration(reg: lt::Registration) -> Option<(String, Vec<WatchedPattern>)> {
	let opts_value = reg.register_options?;
	let opts: lt::DidChangeWatchedFilesRegistrationOptions = match serde_json::from_value(opts_value) {
		Ok(o) => o,
		Err(e) => {
			tracing::warn!(error = %e, "lsp: bad watched-files registration options");
			return None;
		}
	};
	let mut compiled: Vec<WatchedPattern> = Vec::new();
	for watcher in opts.watchers {
		let pattern_str = match watcher.glob_pattern {
			lt::GlobPattern::String(s) => s,
			lt::GlobPattern::Relative(rel) => rel.pattern,
		};
		let glob = match globset::GlobBuilder::new(&pattern_str).literal_separator(false).build() {
			Ok(g) => g,
			Err(e) => {
				tracing::warn!(error = %e, pattern = %pattern_str, "lsp: failed to compile registered glob");
				continue;
			}
		};
		let kind = watcher
			.kind
			.unwrap_or_else(|| lt::WatchKind::Create | lt::WatchKind::Change | lt::WatchKind::Delete);
		compiled.push(WatchedPattern {
			matcher: glob.compile_matcher(),
			kind,
		});
	}
	if compiled.is_empty() {
		return None;
	}
	Some((reg.id, compiled))
}

/// Bridge between host-relative workspace paths and the
/// `file://`-URIs exchanged with the LSP server.
///
/// - `Identity`: host and server see the same absolute paths.
///   `absolutise(rel)` = `host_root.join(rel)`;
///   `relativise(abs)` = `abs.strip_prefix(host_root)`.
/// - `HostMount`: server runs inside a container where the
///   workspace is bind-mounted under a different absolute root
///   (`/workspace/<basename>`). We keep the host root for
///   tree-side lookups and the server root for URI construction,
///   mapping between them symmetrically.
///
/// Only hosts / container variants live here. Remote / SSH
/// targets get their own variant when that lands; the enum is
/// the extension point.
#[derive(Debug, Clone)]
pub enum PathTranslator {
	Identity {
		host_root: Utf8PathBuf,
	},
	HostMount {
		host_root: Utf8PathBuf,
		server_root: Utf8PathBuf,
	},
}

impl PathTranslator {
	/// Host-relative → server-absolute path. Always joined; no
	/// normalisation. The caller is responsible for passing
	/// forward-slash-separated relatives (which is the moon-ide
	/// tree convention already).
	pub fn absolutise(&self, rel_path: &str) -> Utf8PathBuf {
		match self {
			PathTranslator::Identity { host_root } => host_root.join(rel_path),
			PathTranslator::HostMount { server_root, .. } => server_root.join(rel_path),
		}
	}

	/// Server-absolute → host-relative path. Returns `None` when
	/// the URI points outside the workspace (e.g. `rust-analyzer`
	/// publishing a diagnostic against a dep inside the container's
	/// `~/.cargo/registry/…`): the UI has no buffer for those so we
	/// silently drop them.
	pub fn relativise(&self, abs: &Path) -> Option<String> {
		let server_root = match self {
			PathTranslator::Identity { host_root } => host_root.as_std_path(),
			PathTranslator::HostMount { server_root, .. } => server_root.as_std_path(),
		};
		let rel = abs.strip_prefix(server_root).ok()?;
		Some(rel.to_string_lossy().replace('\\', "/"))
	}

	/// Absolute server root — what `initialize.rootUri` and
	/// `workspaceFolders[0].uri` need to point at so the server
	/// opens the right tree.
	pub fn server_root(&self) -> &Utf8PathBuf {
		match self {
			PathTranslator::Identity { host_root } => host_root,
			PathTranslator::HostMount { server_root, .. } => server_root,
		}
	}

	/// Host-side workspace root. Used by callers that need to
	/// cross-reference tree state (e.g. goto-definition response
	/// translation). Never use this for URI construction.
	pub fn host_root(&self) -> &Utf8PathBuf {
		match self {
			PathTranslator::Identity { host_root } => host_root,
			PathTranslator::HostMount { host_root, .. } => host_root,
		}
	}

	/// `true` when the server lives in a different process namespace
	/// from us (devcontainer, future SSH host, …). Callers must NOT
	/// forward our host PID to a server that can't observe it — see
	/// `initialize.processId` in the LSP spec: many servers (tsgo
	/// included) poll that PID with `kill -0` as a parent-died
	/// watchdog. In a separate PID namespace that poll always fails,
	/// so the server exits within seconds and the broker re-spawns
	/// it in a tight loop. Opting out of the watchdog by sending
	/// `null` is the spec-blessed escape hatch for this case.
	pub fn is_remote(&self) -> bool {
		matches!(self, PathTranslator::HostMount { .. })
	}
}

struct DocState {
	version: i32,
}

impl LspServer {
	/// Locate and spawn the server binary. Returns `Ok(None)` when
	/// no copy can be found on disk — caller surfaces a
	/// `NotAvailable` status instead of treating this as an error.
	///
	/// Discovery order for [`LspSpawner::Local`]:
	/// 1. The spec's `DiscoveryStrategy` (project-local
	///    `node_modules/.bin`, or `$CARGO_HOME/bin` etc.).
	/// 2. `$PATH` via `which`.
	///
	/// For [`LspSpawner::DockerExec`] we skip host discovery
	/// entirely — the in-container binary is on the container's
	/// `$PATH` (moon-base handles installation) and we hand
	/// `docker exec` the basename so the container resolves it.
	/// The broker will have already run a `--version` probe to
	/// confirm availability before this spawn.
	pub async fn spawn(
		spec: &LspBinarySpec,
		bin_path: &Path,
		spawner: &LspSpawner,
		translator: PathTranslator,
		workspace_folders: Vec<String>,
		events: broadcast::Sender<LspServerEvent>,
		log_sink: Arc<crate::logs::LogSink>,
	) -> Result<Option<Arc<Self>>, LspClientError> {
		let mut child = spawner
			.build_command(bin_path, spec.args)
			.stdin(Stdio::piped())
			.stdout(Stdio::piped())
			.stderr(Stdio::piped())
			.kill_on_drop(true)
			.spawn()
			.map_err(|e| LspClientError::Io(format!("spawn {}: {}", spec.bin_name, e)))?;

		let stdin = child
			.stdin
			.take()
			.ok_or_else(|| LspClientError::Io("child stdin not piped".into()))?;
		let stdout = child
			.stdout
			.take()
			.ok_or_else(|| LspClientError::Io("child stdout not piped".into()))?;
		let stderr = child
			.stderr
			.take()
			.ok_or_else(|| LspClientError::Io("child stderr not piped".into()))?;

		// Mirror stderr to both `tracing` (debug under
		// `RUST_LOG=moon=debug`) and the bottom-panel logs view
		// (always visible to the user). Server stderr is where
		// crash messages, ill-formed config warnings, and ty-
		// missing-stub gripes live — having those one click away
		// turns an opaque "LSP went quiet" into a clear "the
		// server says X".
		let lang = spec.language_id.to_owned();
		let log_source = format!("lsp.{lang}");
		let stderr_sink = log_sink.clone();
		tokio::spawn(async move {
			let mut reader = BufReader::new(stderr).lines();
			while let Ok(Some(line)) = reader.next_line().await {
				tracing::debug!(lang = %lang, "lsp stderr: {line}");
				stderr_sink.debug(&log_source, format!("stderr: {line}"));
			}
		});

		let (notif_tx, mut notif_rx) = broadcast::channel::<ServerNotification>(64);
		let client = LspClient::spawn(stdin, stdout, notif_tx);

		let server = Arc::new(Self {
			language_id: spec.language_id.to_owned(),
			client,
			child: Mutex::new(Some(child)),
			translator,
			workspace_folders,
			docs: Mutex::new(HashMap::new()),
			watched_patterns: Mutex::new(HashMap::new()),
			completion_resolve_provider: std::sync::atomic::AtomicBool::new(false),
			events: events.clone(),
		});

		// Notification pump: translate the ones we care about,
		// drop the rest. Currently: `textDocument/publishDiagnostics`.
		// Extension slot for `window/showMessage` etc. when we
		// actually surface them.
		let server_ref = server.clone();
		let events_sink = events.clone();
		// Push-diagnostics path. Most LSP servers (rust-analyzer,
		// typescript-language-server) deliver `publishDiagnostics`
		// notifications unsolicited; we forward them straight through.
		// `tsgo` (TypeScript native preview) deliberately does not
		// implement push diagnostics — see
		// <https://github.com/microsoft/typescript-go/issues/2362> —
		// so it relies on the pull path below (`pull_diagnostics`)
		// driven by `update` after every `didOpen` / `didChange`.
		tokio::spawn(async move {
			while let Ok(notif) = notif_rx.recv().await {
				match notif.method.as_str() {
					"textDocument/publishDiagnostics" => {
						let params: lt::PublishDiagnosticsParams = match serde_json::from_value(notif.params) {
							Ok(p) => p,
							Err(e) => {
								tracing::warn!(error = %e, "lsp: bad publishDiagnostics payload");
								continue;
							}
						};
						let Some(path) = server_ref.uri_to_relative(&params.uri) else {
							continue;
						};
						let diagnostics = params.diagnostics.into_iter().map(translate::diagnostic).collect();
						let _ = events_sink.send(LspServerEvent::Diagnostics(mp::LspDiagnosticsEvent {
							path,
							producer: server_ref.language_id.clone(),
							diagnostics,
						}));
					}
					"client/registerCapability" => {
						// Server tells us which capabilities it
						// wants to drive dynamically. We act on
						// `workspace/didChangeWatchedFiles`
						// (record the glob patterns); other
						// methods get logged + dropped, since
						// the spec lets us answer the request
						// with a `null` "applied" reply (the
						// client.rs reader did that already)
						// without actually wiring anything up.
						server_ref.handle_register_capability(notif.params).await;
					}
					"client/unregisterCapability" => {
						server_ref.handle_unregister_capability(notif.params).await;
					}
					"workspace/diagnostic/refresh" => {
						// Server-driven re-pull request. Fires
						// after the server invalidates its
						// per-file diagnostic cache (typically
						// in response to a
						// `workspace/didChangeWatchedFiles`
						// notification we sent it). Re-pull
						// every doc this server has open;
						// `client.rs` already replied null on
						// the wire so the server isn't blocked
						// while we do this.
						server_ref.refresh_open_diagnostics().await;
					}
					_ => {}
				}
			}
		});

		server.initialize().await?;
		events
			.send(LspServerEvent::StatusChanged(mp::LspStatusEvent {
				language_id: spec.language_id.to_owned(),
				status: mp::LspServerStatus::Running,
				detail: Some(bin_path.display().to_string()),
			}))
			.ok();

		// Death-watcher: the moment the client's I/O loops exit
		// (child crashed, stdout EOF, write failed), surface it as
		// a `Crashed` status transition and a log entry. Without
		// this, the death sits invisible until the next request
		// returns the opaque "lsp client shut down" RPC error.
		// The broker's `ensure_server` separately evicts the slot
		// via `is_alive()`, so the very next call re-spawns.
		let death_signal = server.client.death_signal();
		let events_for_death = events.clone();
		let log_for_death = log_sink.clone();
		let lang_for_death = spec.language_id.to_owned();
		let bin_for_death = bin_path.display().to_string();
		tokio::spawn(async move {
			death_signal.wait().await;
			let _ = events_for_death.send(LspServerEvent::StatusChanged(mp::LspStatusEvent {
				language_id: lang_for_death.clone(),
				status: mp::LspServerStatus::Crashed,
				detail: Some("server stdio closed unexpectedly".to_owned()),
			}));
			log_for_death.error(
				&format!("lsp.{lang_for_death}"),
				format!(
					"server died ({bin_for_death}); slot evicted, next request will re-spawn. Inspect lines above for stderr from the dying child."
				),
			);
		});

		Ok(Some(server))
	}

	/// `true` while the underlying I/O loops are still pumping.
	/// The broker calls this when fishing a cached server out of
	/// its slot map — a `false` answer means evict-and-re-spawn.
	pub fn is_alive(&self) -> bool {
		self.client.is_alive()
	}

	async fn initialize(&self) -> Result<(), LspClientError> {
		// Minimal client capabilities. We only claim the features
		// we actually wire up today; adding hover / definition /
		// references later just means flipping a flag here and
		// shipping the command that uses it.
		let caps = lt::ClientCapabilities {
			workspace: Some(lt::WorkspaceClientCapabilities {
				// We support `workspace/didChangeWatchedFiles`
				// (forwarded from the host fs-watcher) but only
				// via dynamic registration — the LSP spec
				// doesn't define a static-watch surface, so the
				// flag below is the only way for the server to
				// learn about file changes. Servers that
				// register watch globs get notified when matching
				// files change on disk; servers that don't
				// register fall back to the per-buffer
				// `didOpen`/`didChange` path same as before.
				did_change_watched_files: Some(lt::DidChangeWatchedFilesClientCapabilities {
					dynamic_registration: Some(true),
					relative_pattern_support: Some(false),
				}),
				// Server-driven diagnostic refresh: the server
				// can ask us to re-pull diagnostics for every
				// open document (e.g. after a watched-files
				// notification invalidated its caches). Wired
				// to `LspServer::refresh_open_diagnostics` in
				// the notification pump.
				diagnostic: Some(lt::DiagnosticWorkspaceClientCapabilities {
					refresh_support: Some(true),
				}),
				// `workspace/configuration`: we declare support so
				// servers that run in pull-config mode (oxlint, in
				// particular, which asks per workspace folder for
				// its `oxc_language_server` settings) get a real
				// reply instead of the request silently failing.
				// `client.rs` answers with an array of empty
				// objects — one per requested item — which oxlint
				// reads as "no per-folder overrides, server, use
				// the on-disk `.oxlintrc.json` you'd have used
				// anyway". Combined with `workspaceFolders` per
				// `.oxlintrc.json`-bearing directory, this is what
				// makes the editor's diagnostics line up with the
				// project's `oxlint --fix` script in monorepos.
				configuration: Some(true),
				..Default::default()
			}),
			text_document: Some(lt::TextDocumentClientCapabilities {
				synchronization: Some(lt::TextDocumentSyncClientCapabilities {
					dynamic_registration: Some(false),
					will_save: Some(false),
					will_save_wait_until: Some(false),
					did_save: Some(false),
				}),
				publish_diagnostics: Some(lt::PublishDiagnosticsClientCapabilities {
					related_information: Some(false),
					tag_support: None,
					version_support: Some(false),
					code_description_support: Some(false),
					data_support: Some(false),
				}),
				hover: Some(lt::HoverClientCapabilities {
					dynamic_registration: Some(false),
					content_format: Some(vec![lt::MarkupKind::Markdown, lt::MarkupKind::PlainText]),
				}),
				definition: Some(lt::GotoCapability {
					dynamic_registration: Some(false),
					// `LocationLink` response lets servers distinguish
					// the full definition range from the identifier
					// span — our translator uses the identifier span
					// so the caret lands right.
					link_support: Some(true),
				}),
				completion: Some(lt::CompletionClientCapabilities {
					dynamic_registration: Some(false),
					completion_item: Some(lt::CompletionItemCapability {
						snippet_support: Some(false),
						documentation_format: Some(vec![lt::MarkupKind::Markdown, lt::MarkupKind::PlainText]),
						insert_replace_support: Some(false),
						// Tell servers we'll call
						// `completionItem/resolve` to hydrate the
						// listed properties on commit. The big one
						// is `additionalTextEdits` — the LSP
						// auto-import pipeline lives there
						// (`tsgo`, `rust-analyzer`, `pyright` all
						// gate the import line on resolve to keep
						// the initial completion list cheap).
						// `documentation` and `detail` are common
						// to lazy-resolve too; declaring them
						// keeps initial results small without
						// losing the side-panel body once the
						// user picks a candidate.
						resolve_support: Some(lt::CompletionItemCapabilityResolveSupport {
							properties: vec![
								"additionalTextEdits".to_string(),
								"documentation".to_string(),
								"detail".to_string(),
							],
						}),
						..Default::default()
					}),
					completion_item_kind: None,
					context_support: Some(true),
					..Default::default()
				}),
				// LSP 3.17 pull-diagnostics. We declare support so
				// servers that prefer pull (notably `tsgo`, which
				// deliberately skips `publishDiagnostics`) know we'll
				// call `textDocument/diagnostic` ourselves after
				// every `didOpen` / `didChange`. Servers that only
				// support push (e.g. `rust-analyzer`) ignore this
				// flag and keep delivering notifications.
				diagnostic: Some(lt::DiagnosticClientCapabilities {
					dynamic_registration: Some(false),
					related_document_support: Some(false),
				}),
				// `textDocument/rename`. `prepare_support: true`
				// tells servers we'll call
				// `textDocument/prepareRename` first so they can
				// gate the rename surface on a real identifier
				// (vs. the frontend's `wordAt` falling back to
				// renaming punctuation tokens). Servers without
				// rename support (clangd's `--background-index=0`,
				// some bespoke language servers) silently ignore
				// the flag and return `null` for both requests —
				// the frontend treats that as "not renameable",
				// no toast.
				rename: Some(lt::RenameClientCapabilities {
					dynamic_registration: Some(false),
					prepare_support: Some(true),
					prepare_support_default_behavior: None,
					honors_change_annotations: Some(false),
				}),
				..Default::default()
			}),
			..Default::default()
		};

		// Initialize with the **server-side** root — container
		// servers see `/workspace/<basename>`, host servers see
		// the host absolute path. The translator bridges which
		// one we're talking to; the LSP surface is agnostic.
		let server_root = self.translator.server_root();
		let root_uri = path_to_file_uri(server_root.as_std_path());

		// Build one `WorkspaceFolder` per host-relative entry in
		// `self.workspace_folders`. Empty / `"."` entries map to
		// the workspace root itself; anything else translates
		// through `absolutise` so container-routed servers see the
		// `/workspace/<basename>/<rel>` form. Folder name is the
		// basename — what shows up in the server's logs and
		// status output. Falls back to the root if the field is
		// somehow empty so we never send `Some(vec![])`, which
		// some servers treat as "no folders, work file-by-file".
		let mut folders: Vec<lt::WorkspaceFolder> = self
			.workspace_folders
			.iter()
			.map(|rel| {
				let abs = if rel.is_empty() || rel == "." {
					server_root.clone()
				} else {
					self.translator.absolutise(rel)
				};
				let name = abs.file_name().unwrap_or("workspace").to_owned();
				lt::WorkspaceFolder {
					uri: path_to_file_uri(abs.as_std_path()),
					name,
				}
			})
			.collect();
		if folders.is_empty() {
			folders.push(lt::WorkspaceFolder {
				uri: root_uri.clone(),
				name: server_root.file_name().unwrap_or("workspace").to_owned(),
			});
		}

		// Forward our PID only if the server is in the same PID
		// namespace (host route). For container / remote routes we
		// send `null`: the spec lets us opt out of the parent-died
		// watchdog, and forwarding a PID the server can't see is
		// actively harmful — tsgo's watchdog poll `kill -0` fails
		// immediately, the server exits ~5s into its lifetime, and
		// the broker re-spawns in a tight loop ("Parent process N
		// has exited, shutting down" / "context canceled" in stderr).
		let process_id = if self.translator.is_remote() {
			None
		} else {
			Some(std::process::id())
		};

		#[allow(deprecated)]
		let params = lt::InitializeParams {
			process_id,
			root_path: None,
			root_uri: Some(root_uri.clone()),
			initialization_options: None,
			capabilities: caps,
			trace: None,
			workspace_folders: Some(folders),
			client_info: Some(lt::ClientInfo {
				name: "moon-ide".into(),
				version: Some(env!("CARGO_PKG_VERSION").into()),
			}),
			locale: None,
			work_done_progress_params: lt::WorkDoneProgressParams::default(),
		};

		let result: lt::InitializeResult = self.client.request("initialize", params).await?;
		// Stash the server's `completionProvider.resolveProvider`
		// flag so the broker knows whether a
		// `completionItem/resolve` round-trip is going to give us
		// anything new. `tsgo` / `rust-analyzer` / `pyright`
		// advertise `true` and use the resolve hook to ship the
		// auto-import line; servers that don't (some bespoke
		// language servers, older clangd) advertise `false` and
		// the broker short-circuits resolve.
		let resolve_provider = result
			.capabilities
			.completion_provider
			.as_ref()
			.and_then(|cp| cp.resolve_provider)
			.unwrap_or(false);
		self
			.completion_resolve_provider
			.store(resolve_provider, std::sync::atomic::Ordering::Relaxed);
		self.client.notify("initialized", lt::InitializedParams {}).await?;
		Ok(())
	}

	/// Whether this server told us at `initialize` time that
	/// `completionItem/resolve` is supported. Used by the broker
	/// to decide whether the resolve token an `LspCompletionItem`
	/// carries is worth shipping to the frontend at all.
	pub fn supports_completion_resolve(&self) -> bool {
		self
			.completion_resolve_provider
			.load(std::sync::atomic::Ordering::Relaxed)
	}

	/// Send `textDocument/didOpen` (first time) or
	/// `textDocument/didChange` (subsequent calls) for `rel_path`,
	/// then schedule a diagnostic pull. The two paths are merged on
	/// purpose: a respawned server starts with an empty `docs` map,
	/// and the very next thing the broker forwards may be either a
	/// fresh `lsp_open` (tab switch / file load) or a debounced
	/// `lsp_update` from in-flight typing — keeping them on the
	/// same entry point means the new server picks up the buffer no
	/// matter which one wins the race. The frontend's
	/// [`super::broker::LspBroker::update`] forwards through here
	/// for exactly that reason; treating an `update` as a "didOpen
	/// if needed" is what unsticks the linter co-tenant after a
	/// crash or manual restart.
	pub async fn open(self: &Arc<Self>, rel_path: &str, text: String, language_id: &str) -> Result<(), LspClientError> {
		let mut docs = self.docs.lock().await;
		let uri = self.relative_to_uri(rel_path);
		if let Some(state) = docs.get_mut(rel_path) {
			state.version += 1;
			let version = state.version;
			drop(docs);
			self.apply_change(&uri, version, text).await?;
			self.spawn_pull_diagnostics(rel_path);
			return Ok(());
		}
		let version = 1;
		let params = lt::DidOpenTextDocumentParams {
			text_document: lt::TextDocumentItem {
				uri,
				language_id: language_id.to_owned(),
				version,
				text,
			},
		};
		docs.insert(rel_path.to_owned(), DocState { version });
		drop(docs);
		self.client.notify("textDocument/didOpen", params).await?;
		self.spawn_pull_diagnostics(rel_path);
		Ok(())
	}

	/// Fire `textDocument/diagnostic` for `rel_path` in a detached
	/// task. Callers don't await — the pull is best-effort and must
	/// not block the originating `didOpen` / `didChange` notification
	/// path. Servers that don't implement pull diagnostics return
	/// `MethodNotFound`, which we silently ignore (push diagnostics
	/// are the fallback for those servers).
	///
	/// The `result_id` round-trip from a previous pull is **not**
	/// threaded through yet; servers that reuse it gain "did anything
	/// change since last pull?" caching, but for our minimal slice
	/// the unconditional full pull keeps the implementation small —
	/// extension slot when latency matters.
	fn spawn_pull_diagnostics(self: &Arc<Self>, rel_path: &str) {
		let server = Arc::clone(self);
		let path = rel_path.to_owned();
		tokio::spawn(async move {
			if let Err(err) = server.pull_diagnostics(&path).await {
				// `MethodNotFound` (-32601) from servers that don't
				// implement pull is the expected fallback path for
				// `rust-analyzer` and any other push-only server;
				// quiet at debug rather than warn-spam every save.
				match &err {
					LspClientError::Rpc(rpc) if rpc.code == -32601 => {
						tracing::debug!(path = %path, "lsp: pull diagnostics unsupported (push-only server)");
					}
					_ => {
						tracing::debug!(path = %path, %err, "lsp: pull diagnostics failed");
					}
				}
			}
		});
	}

	async fn pull_diagnostics(&self, rel_path: &str) -> Result<(), LspClientError> {
		let uri = self.relative_to_uri(rel_path);
		// Hand-shaped params instead of `lt::DocumentDiagnosticParams`:
		// `lsp-types`'s definition serialises `Option::None` as
		// `null`, but tsgo's Go-side protobuf decoder rejects that
		// for the `identifier` and `previousResultId` fields with
		// "null value is not allowed for field". The LSP spec says
		// these are optional and absent ≠ null; we serialise with
		// `skip_serializing_if` to honour that.
		#[derive(serde::Serialize)]
		#[serde(rename_all = "camelCase")]
		struct DiagnosticParams<'a> {
			text_document: &'a lt::TextDocumentIdentifier,
			#[serde(skip_serializing_if = "Option::is_none")]
			identifier: Option<&'a str>,
			#[serde(skip_serializing_if = "Option::is_none")]
			previous_result_id: Option<&'a str>,
		}
		let text_document = lt::TextDocumentIdentifier { uri };
		let params = DiagnosticParams {
			text_document: &text_document,
			identifier: None,
			previous_result_id: None,
		};
		let resp: lt::DocumentDiagnosticReportResult = self.client.request("textDocument/diagnostic", params).await?;
		let items = match resp {
			lt::DocumentDiagnosticReportResult::Report(lt::DocumentDiagnosticReport::Full(full)) => {
				full.full_document_diagnostic_report.items
			}
			// `Unchanged` means the server has nothing new to say
			// since the last pull — we keep whatever the UI is
			// currently showing instead of clobbering it.
			lt::DocumentDiagnosticReportResult::Report(lt::DocumentDiagnosticReport::Unchanged(_)) => {
				return Ok(());
			}
			// Streaming response; we don't wire the partial-result
			// channel today. Servers can fall back to a single
			// `Full` payload, which the branch above handles.
			lt::DocumentDiagnosticReportResult::Partial(_) => {
				return Ok(());
			}
		};
		let diagnostics = items.into_iter().map(translate::diagnostic).collect();
		let _ = self.events.send(LspServerEvent::Diagnostics(mp::LspDiagnosticsEvent {
			path: rel_path.to_owned(),
			producer: self.language_id.clone(),
			diagnostics,
		}));
		Ok(())
	}

	async fn apply_change(&self, uri: &lt::Uri, version: i32, text: String) -> Result<(), LspClientError> {
		// Full-document sync: one content change covering the whole
		// buffer. We don't tell the server about incremental edits
		// because we don't advertise incremental sync in
		// initialize's `TextDocumentSyncClientCapabilities`, so the
		// server expects full bodies regardless of what we'd prefer.
		let params = lt::DidChangeTextDocumentParams {
			text_document: lt::VersionedTextDocumentIdentifier {
				uri: uri.clone(),
				version,
			},
			content_changes: vec![lt::TextDocumentContentChangeEvent {
				range: None,
				range_length: None,
				text,
			}],
		};
		self.client.notify("textDocument/didChange", params).await
	}

	/// Process one `client/registerCapability` request from the
	/// server. Today the only registration we act on is
	/// `workspace/didChangeWatchedFiles`; everything else is
	/// already covered by the static capability set we sent in
	/// `initialize`, so we log and drop. Per the LSP spec, the
	/// `null` reply [`client.rs`] already wrote on the wire is a
	/// "registration applied" success — even if our action below
	/// fails to compile a glob, the server proceeds as if it had.
	///
	/// The default `kind` for a watcher is 7 (Create | Change |
	/// Delete) per spec, applied when the server omits the field.
	async fn handle_register_capability(self: &Arc<Self>, params: Value) {
		let parsed: lt::RegistrationParams = match serde_json::from_value(params) {
			Ok(p) => p,
			Err(e) => {
				tracing::warn!(error = %e, "lsp: bad registerCapability payload");
				return;
			}
		};
		for reg in parsed.registrations {
			if reg.method != "workspace/didChangeWatchedFiles" {
				tracing::debug!(method = %reg.method, "lsp: ignoring dynamic registration");
				continue;
			}
			match parse_watched_files_registration(reg) {
				Some((id, patterns)) => {
					let count = patterns.len();
					self.watched_patterns.lock().await.insert(id.clone(), patterns);
					// Trace, not debug: tsgo re-registers one
					// watcher per (id, pattern) on every snapshot
					// update, so a folder-switch + tab-restore
					// burst can fire this dozens of times per
					// second. Debug-level enrolment of the
					// initial registration would be useful;
					// per-replace spam isn't.
					tracing::trace!(lang = %self.language_id, registration_id = %id, watchers = count, "lsp: recorded watched-files registration");
				}
				None => {
					tracing::debug!(lang = %self.language_id, "lsp: skipping watched-files registration with no usable patterns");
				}
			}
		}
	}

	/// Drop registrations that the server no longer wants. Same
	/// shape as [`handle_register_capability`] but in reverse. We
	/// only forget the entry — the next `registerCapability` with
	/// the same id will replace it. Failures here are silently
	/// dropped: an unregister of an unknown id is a no-op anyway.
	async fn handle_unregister_capability(self: &Arc<Self>, params: Value) {
		let parsed: lt::UnregistrationParams = match serde_json::from_value(params) {
			Ok(p) => p,
			Err(e) => {
				tracing::warn!(error = %e, "lsp: bad unregisterCapability payload");
				return;
			}
		};
		let mut guard = self.watched_patterns.lock().await;
		for unreg in parsed.unregisterations {
			if unreg.method != "workspace/didChangeWatchedFiles" {
				continue;
			}
			guard.remove(&unreg.id);
		}
	}

	/// Forward an fs-watcher batch to the server as one
	/// `workspace/didChangeWatchedFiles` notification, after
	/// filtering paths through the globs the server registered
	/// for that event. No-op when this server hasn't registered
	/// any watchers (rust-analyzer post-init, servers that don't
	/// implement the capability at all) or when none of the
	/// changed paths match — the round-trip cost is one map walk
	/// against the registered globs.
	///
	/// `kind` is hardcoded to `Changed` for now: the host
	/// fs-watcher emits a flat `paths` list without per-path
	/// create/modify/delete classification, and `Changed` is what
	/// every wired server actually keys off (rust-analyzer
	/// invalidates caches, tsgo / tsserver re-index). If a
	/// fidelity issue surfaces, the watcher payload extension is
	/// the right fix; defaulting here avoids over-engineering
	/// before there's a real bug.
	pub async fn notify_files_changed(&self, paths: &[String]) -> Result<(), LspClientError> {
		if paths.is_empty() {
			return Ok(());
		}
		let patterns = self.watched_patterns.lock().await;
		if patterns.is_empty() {
			return Ok(());
		}
		let kind = lt::FileChangeType::CHANGED;
		let mut events: Vec<lt::FileEvent> = Vec::new();
		for path in paths {
			let mut matched = false;
			for group in patterns.values() {
				for pattern in group {
					if !pattern.kind.contains(lt::WatchKind::Change) {
						continue;
					}
					if pattern.matcher.is_match(path) {
						matched = true;
						break;
					}
				}
				if matched {
					break;
				}
			}
			if !matched {
				continue;
			}
			events.push(lt::FileEvent {
				uri: self.relative_to_uri(path),
				typ: kind,
			});
		}
		drop(patterns);
		if events.is_empty() {
			return Ok(());
		}
		let count = events.len();
		let params = lt::DidChangeWatchedFilesParams { changes: events };
		self.client.notify("workspace/didChangeWatchedFiles", params).await?;
		tracing::trace!(lang = %self.language_id, count, "lsp: forwarded watched-files batch");
		Ok(())
	}

	/// Re-pull diagnostics for every document currently open on
	/// this server. Used to refresh stale diagnostics after an
	/// out-of-band file change (a `git checkout` rewriting source
	/// files, an external editor save, …) — none of which fires
	/// `didChange` for already-open buffers, so without this nudge
	/// the panel keeps painting the diagnostics it computed
	/// against the previous version of the file.
	///
	/// Best-effort: each pull runs in a detached task via the
	/// existing `spawn_pull_diagnostics` plumbing, so the caller
	/// returns immediately and individual server failures don't
	/// block a workspace-wide refresh. Servers that don't
	/// implement pull diagnostics (push-only, e.g. rust-analyzer)
	/// already noop the pull at debug-log level inside
	/// `pull_diagnostics`, so this method is cheap to call across
	/// every running server regardless of which ones actually
	/// answer.
	pub async fn refresh_open_diagnostics(self: &Arc<Self>) {
		let paths: Vec<String> = {
			let docs = self.docs.lock().await;
			docs.keys().cloned().collect()
		};
		for path in paths {
			self.spawn_pull_diagnostics(&path);
		}
	}

	pub async fn close(&self, rel_path: &str) -> Result<(), LspClientError> {
		let removed = self.docs.lock().await.remove(rel_path);
		if removed.is_none() {
			return Ok(());
		}
		let uri = self.relative_to_uri(rel_path);
		let params = lt::DidCloseTextDocumentParams {
			text_document: lt::TextDocumentIdentifier { uri },
		};
		self.client.notify("textDocument/didClose", params).await
	}

	pub async fn hover(&self, rel_path: &str, position: mp::LspPosition) -> Result<Option<mp::LspHover>, LspClientError> {
		let uri = self.relative_to_uri(rel_path);
		let params = lt::HoverParams {
			text_document_position_params: lt::TextDocumentPositionParams {
				text_document: lt::TextDocumentIdentifier { uri },
				position: translate::to_lsp_position(position),
			},
			work_done_progress_params: lt::WorkDoneProgressParams::default(),
		};
		let resp: Option<lt::Hover> = self.client.request("textDocument/hover", params).await?;
		Ok(resp.and_then(translate::hover))
	}

	/// Send `textDocument/definition`. Returns `None` if the server
	/// didn't know where the symbol was (common for literals,
	/// whitespace, arbitrary hovers) — not an error, just a
	/// silent-skip signal the UI uses to leave the identifier
	/// un-underlined.
	pub async fn definition(
		&self,
		rel_path: &str,
		position: mp::LspPosition,
	) -> Result<Option<mp::LspLocation>, LspClientError> {
		let uri = self.relative_to_uri(rel_path);
		let params = lt::GotoDefinitionParams {
			text_document_position_params: lt::TextDocumentPositionParams {
				text_document: lt::TextDocumentIdentifier { uri },
				position: translate::to_lsp_position(position),
			},
			work_done_progress_params: lt::WorkDoneProgressParams::default(),
			partial_result_params: lt::PartialResultParams::default(),
		};
		let resp: Option<lt::GotoDefinitionResponse> = self.client.request("textDocument/definition", params).await?;
		// The server reports URIs in its own filesystem view
		// (container paths under `HostMount`), so strip against
		// the server root — otherwise every goto-def inside a
		// containerised project would be mis-classified as
		// "external" and surface as a toast.
		Ok(resp.and_then(|r| translate::definition_response(r, self.translator.server_root().as_std_path())))
	}

	/// Send `textDocument/prepareRename`. Returns `None` when the
	/// server says the cursor isn't on a renameable symbol — the
	/// UI treats that as a quiet "no rename here", no toast.
	/// `fallback_word` is the identifier the frontend identified
	/// under the cursor; we hand it back as the input placeholder
	/// when the server returned a bare range (the common shape).
	pub async fn prepare_rename(
		&self,
		rel_path: &str,
		position: mp::LspPosition,
		fallback_word: &str,
	) -> Result<Option<mp::LspPrepareRename>, LspClientError> {
		let uri = self.relative_to_uri(rel_path);
		let params = lt::TextDocumentPositionParams {
			text_document: lt::TextDocumentIdentifier { uri },
			position: translate::to_lsp_position(position),
		};
		let resp: Option<lt::PrepareRenameResponse> = self.client.request("textDocument/prepareRename", params).await?;
		Ok(resp.and_then(|r| translate::prepare_rename_response(r, fallback_word)))
	}

	/// Send `textDocument/rename`. Returns an empty
	/// [`mp::LspWorkspaceEdit`] when the server has nothing to
	/// change (cursor wasn't on a real symbol, server doesn't
	/// support rename) — the frontend treats zero edits the same
	/// as a `None` from `prepare_rename`.
	pub async fn rename(
		&self,
		rel_path: &str,
		position: mp::LspPosition,
		new_name: &str,
	) -> Result<mp::LspWorkspaceEdit, LspClientError> {
		let uri = self.relative_to_uri(rel_path);
		let params = lt::RenameParams {
			text_document_position: lt::TextDocumentPositionParams {
				text_document: lt::TextDocumentIdentifier { uri },
				position: translate::to_lsp_position(position),
			},
			new_name: new_name.to_owned(),
			work_done_progress_params: lt::WorkDoneProgressParams::default(),
		};
		let resp: Option<lt::WorkspaceEdit> = self.client.request("textDocument/rename", params).await?;
		// Same `server_root` reasoning as `definition`: container
		// servers emit URIs under their mount root, so we strip
		// against the translator's server view, not the host root.
		Ok(match resp {
			Some(edit) => translate::workspace_edit(edit, self.translator.server_root().as_std_path()),
			None => mp::LspWorkspaceEdit::default(),
		})
	}

	/// Send `textDocument/codeAction` for one diagnostic the user
	/// is parked on. We pass exactly that diagnostic in the request
	/// `context`, with `only` narrowed to `quickfix` so servers that
	/// also return refactor / source actions stay out of the lint
	/// tooltip — those belong on a different surface (a future
	/// `Show all code actions` keybinding) and would crowd out the
	/// fix-this-thing options the user actually came for.
	///
	/// Edits in the response are translated through
	/// [`translate::workspace_edit`] against the translator's
	/// server-side root, so a containerised oxlint that sees
	/// `/workspace/<basename>/...` URIs comes back with paths
	/// relative to the host workspace root — exactly the shape the
	/// frontend's open-buffer / fs-write appliers already accept.
	pub async fn code_action(
		&self,
		rel_path: &str,
		range: &mp::LspRange,
		diagnostic: &mp::LspDiagnostic,
		producer: &str,
	) -> Result<Vec<mp::LspCodeAction>, LspClientError> {
		let uri = self.relative_to_uri(rel_path);
		let params = lt::CodeActionParams {
			text_document: lt::TextDocumentIdentifier { uri },
			range: translate::to_lsp_range(range),
			context: lt::CodeActionContext {
				diagnostics: vec![translate::to_lsp_diagnostic(diagnostic)],
				only: Some(vec![lt::CodeActionKind::QUICKFIX]),
				trigger_kind: Some(lt::CodeActionTriggerKind::INVOKED),
			},
			work_done_progress_params: lt::WorkDoneProgressParams::default(),
			partial_result_params: lt::PartialResultParams::default(),
		};
		let resp: Option<lt::CodeActionResponse> = self.client.request("textDocument/codeAction", params).await?;
		let Some(resp) = resp else {
			return Ok(Vec::new());
		};
		Ok(translate::code_actions(
			resp,
			producer,
			self.translator.server_root().as_std_path(),
		))
	}

	pub async fn completion(
		&self,
		rel_path: &str,
		position: mp::LspPosition,
	) -> Result<mp::LspCompletionList, LspClientError> {
		let uri = self.relative_to_uri(rel_path);
		let params = lt::CompletionParams {
			text_document_position: lt::TextDocumentPositionParams {
				text_document: lt::TextDocumentIdentifier { uri },
				position: translate::to_lsp_position(position),
			},
			work_done_progress_params: lt::WorkDoneProgressParams::default(),
			partial_result_params: lt::PartialResultParams::default(),
			context: Some(lt::CompletionContext {
				trigger_kind: lt::CompletionTriggerKind::INVOKED,
				trigger_character: None,
			}),
		};
		let resp: Option<lt::CompletionResponse> = self.client.request("textDocument/completion", params).await?;
		let supports_resolve = self.supports_completion_resolve();
		Ok(match resp {
			Some(r) => translate::completion_response_with_resolve(r, supports_resolve),
			None => mp::LspCompletionList {
				is_incomplete: false,
				items: vec![],
			},
		})
	}

	/// Hand a previously-emitted `LspCompletionItem` back to the
	/// server via `completionItem/resolve` to fetch the lazy-
	/// resolved fields — primarily the auto-import block in
	/// `additionalTextEdits`. The frontend ships back the opaque
	/// `resolve_token` we issued in [`Self::completion`]; we
	/// JSON-decode it into the original `lt::CompletionItem`,
	/// round-trip through the server, and re-project. When the
	/// server didn't advertise `resolveProvider`, this is a no-op
	/// — we just decode the token and return its projection so
	/// the frontend doesn't need a second branch.
	pub async fn completion_resolve(&self, resolve_token: &str) -> Result<mp::LspCompletionItem, LspClientError> {
		let original: lt::CompletionItem =
			serde_json::from_str(resolve_token).map_err(|e| LspClientError::Decode(format!("resolve token: {e}")))?;
		if !self.supports_completion_resolve() {
			// Decoding the token alone doesn't add `additionalTextEdits`
			// the server didn't already send, so the round-trip
			// would be a no-op; skip the IPC and re-project.
			// `include_resolve_token: false` so the frontend can
			// stop chasing resolve on the same item — we already
			// know there's nothing more to fetch.
			return Ok(translate::completion_item_with_resolve(original, false));
		}
		let resolved: lt::CompletionItem = self.client.request("completionItem/resolve", original).await?;
		Ok(translate::completion_item_with_resolve(resolved, false))
	}

	pub fn language_id(&self) -> &str {
		&self.language_id
	}

	/// Graceful shutdown: send `shutdown` request, then `exit`
	/// notification. Caller should drop the server afterwards; the
	/// child has `kill_on_drop` so even a hung server can't outlive
	/// the broker.
	pub async fn shutdown(&self) {
		// Best-effort; we don't want a hung server to keep the
		// broker from tearing down. A 2s budget is plenty for any
		// sane LSP.
		let shutdown_fut = self
			.client
			.request::<serde_json::Value, serde_json::Value>("shutdown", serde_json::Value::Null);
		let _ = tokio::time::timeout(std::time::Duration::from_secs(2), shutdown_fut).await;
		let _ = self.client.notify("exit", serde_json::Value::Null).await;
		self.client.shutdown().await;
		if let Some(mut child) = self.child.lock().await.take() {
			let _ = tokio::time::timeout(std::time::Duration::from_secs(2), child.wait()).await;
		}
	}

	fn relative_to_uri(&self, rel_path: &str) -> lt::Uri {
		let abs = self.translator.absolutise(rel_path);
		path_to_file_uri(abs.as_std_path())
	}

	fn uri_to_relative(&self, uri: &lt::Uri) -> Option<String> {
		// `lsp_types::Uri` is a `fluent_uri` newtype and doesn't
		// expose `to_file_path`; `url::Url` does, and the LSP string
		// form is exactly a URL, so parse and delegate. Both crates
		// accept the same file URI syntax.
		let parsed = url::Url::parse(uri.as_str()).ok()?;
		let path = parsed.to_file_path().ok()?;
		self.translator.relativise(&path)
	}
}

/// Locate `bin_name` on disk, preferring an ecosystem-idiomatic
/// location over a bare `$PATH` lookup.
///
/// Returns `None` when nothing is found — caller treats that as
/// `NotAvailable` and surfaces the spec's `install_hint`.
///
/// Platform note: Node's `.bin` entry is a symlink to the real
/// script on *nix (shebang resolved by the kernel) and a `.cmd`
/// wrapper on Windows. We pick the right suffix so
/// `tokio::process::Command` gets a spawn-able path on both. Cargo's
/// `bin/` has no such dance — both targets install a native executable
/// (with `.exe` on Windows, which `which`-style resolution and the
/// explicit check below both handle).
pub fn discover_binary(bin_name: &str, strategy: DiscoveryStrategy, start: &Path) -> Option<PathBuf> {
	match strategy {
		DiscoveryStrategy::NodeModules => {
			discover_node_modules_local(bin_name, start).or_else(|| match which::which(bin_name) {
				Ok(path) => {
					tracing::debug!(bin = bin_name, path = %path.display(), "lsp: resolved via PATH");
					Some(path)
				}
				Err(_) => None,
			})
		}
		DiscoveryStrategy::CargoHome => {
			// 1. Ask rustup directly. When the tool is a rustup
			//    component that's installed, this returns the real
			//    toolchain binary path (bypassing the shim in
			//    `~/.cargo/bin/`). When the component isn't installed,
			//    `rustup which` exits non-zero and we fall through —
			//    critically, we also want to avoid spawning the raw
			//    shim here because it'd die at startup with
			//    `Unknown binary '<tool>' in official toolchain`,
			//    which the broker would report as "Crashed" instead
			//    of the more useful "install this component" hint.
			if let Some(path) = rustup_which(bin_name) {
				tracing::debug!(bin = bin_name, path = %path.display(), "lsp: resolved via rustup which");
				return Some(path);
			}
			// 2. `cargo install` builds land in `~/.cargo/bin/` as
			//    real executables (not symlinks to rustup). Accept
			//    those; reject anything that looks like a rustup
			//    shim for the reason above.
			if let Some(candidate) = cargo_bin_candidate(bin_name) {
				if candidate.exists() && !is_rustup_shim(&candidate) {
					tracing::debug!(
						bin = bin_name,
						path = %candidate.display(),
						"lsp: resolved via cargo home"
					);
					return Some(candidate);
				}
			}
			// 3. `$PATH` — last resort for package-manager installs
			//    or hand-compiled binaries living outside cargo-home.
			//    Still filter out rustup shims here: `~/.cargo/bin/`
			//    is typically on `$PATH`, so a naive `which` will
			//    happily hand us the same broken shim.
			match which::which(bin_name) {
				Ok(path) if !is_rustup_shim(&path) => {
					tracing::debug!(bin = bin_name, path = %path.display(), "lsp: resolved via PATH");
					Some(path)
				}
				_ => None,
			}
		}
		DiscoveryStrategy::PythonVenv => {
			let (subdir, filename) = python_venv_layout(bin_name);
			for ancestor in start.ancestors() {
				let candidate = ancestor.join(".venv").join(subdir).join(&filename);
				if candidate.exists() {
					tracing::debug!(
						bin = bin_name,
						path = %candidate.display(),
						"lsp: resolved via project-local .venv"
					);
					return Some(candidate);
				}
			}
			match which::which(bin_name) {
				Ok(path) => {
					tracing::debug!(bin = bin_name, path = %path.display(), "lsp: resolved via PATH");
					Some(path)
				}
				Err(_) => None,
			}
		}
		DiscoveryStrategy::GoBin => {
			// 1. `$GOBIN/<bin>` — explicit override the Go
			//    toolchain itself honours over `$GOPATH/bin`.
			// 2. `$GOPATH/bin/<bin>` — what `go install` writes
			//    to when `$GOBIN` isn't set. `$GOPATH` itself
			//    defaults to `$HOME/go` per the Go docs (no
			//    longer required to be set since Go 1.8).
			// 3. `$PATH` — distro packages (`golang-go`),
			//    Homebrew, or hand-compiled installs.
			if let Some(candidate) = go_bin_candidate(bin_name) {
				if candidate.exists() {
					tracing::debug!(
						bin = bin_name,
						path = %candidate.display(),
						"lsp: resolved via go bin"
					);
					return Some(candidate);
				}
			}
			match which::which(bin_name) {
				Ok(path) => {
					tracing::debug!(bin = bin_name, path = %path.display(), "lsp: resolved via PATH");
					Some(path)
				}
				Err(_) => None,
			}
		}
	}
}

/// Project-local half of the `NodeModules` strategy — the two
/// filesystem tiers without the trailing `$PATH` lookup. Split out
/// so [`discover_server_binary`] can rank "project-local other
/// binary" (TS 7's `tsc`) above "global copy of the preferred one".
fn discover_node_modules_local(bin_name: &str, start: &Path) -> Option<PathBuf> {
	let filename = if cfg!(windows) {
		format!("{bin_name}.cmd")
	} else {
		bin_name.to_owned()
	};
	// Tier 1: hoisted layout — walk ancestors of `start`
	// looking for `node_modules/.bin/<bin>`. Hits the
	// common case (single-package projects, npm / pnpm /
	// bun monorepos with a hoisted root `node_modules`).
	for ancestor in start.ancestors() {
		let candidate = ancestor.join("node_modules").join(".bin").join(&filename);
		if candidate.exists() {
			tracing::debug!(
				bin = bin_name,
				path = %candidate.display(),
				"lsp: resolved via project-local node_modules"
			);
			return Some(candidate);
		}
	}
	// Tier 2: per-workspace layout — pnpm / yarn workspaces
	// with `nodeLinker: node-modules` install each
	// package's deps into its own `node_modules/.bin/`.
	// `start` is the IDE workspace root (e.g.
	// `~/repo/`), but the closest `node_modules/.bin/`
	// might live at `apps/api/node_modules/.bin/`. Walk a
	// bounded subtree of `start` looking for those nested
	// locations. Capped depth keeps a `node_modules`-free
	// monorepo from costing a full filesystem walk on
	// every spawn — typical hits are 2-3 levels deep
	// (`apps/<pkg>/`, `packages/<pkg>/`).
	if let Some(found) = scan_for_node_modules_bin(start, &filename, NODE_MODULES_SCAN_DEPTH) {
		tracing::debug!(
			bin = bin_name,
			path = %found.display(),
			"lsp: resolved via nested workspace node_modules"
		);
		return Some(found);
	}
	None
}

/// Spec-aware entry point for host binary discovery. Every server
/// but TypeScript resolves purely by `bin_name` + strategy
/// ([`discover_binary`]); TypeScript inserts one extra tier between
/// "project-local `tsgo`" and "`tsgo` on `$PATH`": a project-local
/// native `tsc` from `typescript@7+`. Ranking a project-pinned TS 7
/// above a global `tsgo` keeps the invariant that the project's own
/// toolchain always wins.
pub fn discover_server_binary(spec: &LspBinarySpec, start: &Path) -> Option<PathBuf> {
	if spec.language_id != TS_SERVER.language_id {
		return discover_binary(spec.bin_name, spec.discovery, start);
	}
	if let Some(path) = discover_node_modules_local(spec.bin_name, start) {
		return Some(path);
	}
	if let Some(path) = discover_ts7_tsc(start) {
		tracing::debug!(
			path = %path.display(),
			"lsp: resolved via project-local typescript@7 tsc"
		);
		return Some(path);
	}
	match which::which(spec.bin_name) {
		Ok(path) => {
			tracing::debug!(bin = spec.bin_name, path = %path.display(), "lsp: resolved via PATH");
			Some(path)
		}
		Err(_) => None,
	}
}

/// TS 7 moved the native compiler into the mainline `typescript`
/// package and renamed the binary `tsgo` → `tsc`; the same binary
/// speaks `--lsp --stdio`. A project on `typescript@7` therefore
/// gets LSP without installing `@typescript/native-preview` — but
/// the version gate is load-bearing: `typescript@6`'s `tsc` is the
/// JS compiler with no `--lsp` mode, so a bare `.bin/tsc` hit is
/// not enough. We read the resolved
/// `node_modules/typescript/package.json` sitting next to the
/// `.bin` entry and require major ≥ 7.
///
/// No `$PATH` tier here: a global `tsc`'s version can't be checked
/// with a cheap manifest read, and a global TS 7 install is not a
/// shape the team uses.
fn discover_ts7_tsc(start: &Path) -> Option<PathBuf> {
	let filename = if cfg!(windows) { "tsc.cmd" } else { "tsc" };
	for ancestor in start.ancestors() {
		if let Some(found) = ts7_tsc_in(&ancestor.join("node_modules"), filename) {
			return Some(found);
		}
	}
	nested_ts7_tsc(start, filename)
}

/// Nested-workspace tier of [`discover_ts7_tsc`]: the first
/// `*/node_modules/.bin/tsc` the bounded scan finds, version-gated.
/// First-match semantics — a monorepo mixing `typescript@6` and
/// `typescript@7` across packages resolves whichever the scan hits
/// first, same posture as [`scan_for_node_modules_bin`] itself.
fn nested_ts7_tsc(root: &Path, filename: &str) -> Option<PathBuf> {
	let nested = scan_for_node_modules_bin(root, filename, NODE_MODULES_SCAN_DEPTH)?;
	let node_modules = nested.parent()?.parent()?;
	(typescript_major_version(node_modules)? >= 7).then_some(nested)
}

/// `node_modules/.bin/<filename>` inside `node_modules`, accepted
/// only when the sibling `typescript` package is major ≥ 7.
fn ts7_tsc_in(node_modules: &Path, filename: &str) -> Option<PathBuf> {
	let candidate = node_modules.join(".bin").join(filename);
	if !candidate.exists() {
		return None;
	}
	(typescript_major_version(node_modules)? >= 7).then_some(candidate)
}

/// Major version of the `typescript` package resolved at
/// `node_modules/typescript/package.json`, or `None` when the
/// package (or a parseable version) isn't there. Follows symlinks,
/// so pnpm's `.pnpm/`-backed layout reads the real manifest.
fn typescript_major_version(node_modules: &Path) -> Option<u64> {
	let manifest = std::fs::read_to_string(node_modules.join("typescript").join("package.json")).ok()?;
	let json: serde_json::Value = serde_json::from_str(&manifest).ok()?;
	json.get("version")?.as_str()?.split('.').next()?.parse().ok()
}

/// How deep we'll scan a workspace root for nested
/// `node_modules/.bin/<bin>` entries before giving up. Picked to
/// cover the common monorepo shapes (`apps/<pkg>/`,
/// `packages/<pkg>/`, `services/<pkg>/`) with a tiny buffer for
/// odd ones (`apps/<area>/<pkg>/`). Past this depth the cost of
/// a fresh discovery on every LSP spawn outweighs the benefit:
/// the user can hoist their dep, install it globally, or move
/// their workspace root to the package that owns the binary.
const NODE_MODULES_SCAN_DEPTH: u32 = 4;

/// Bounded depth-first scan of `root` looking for
/// `*/node_modules/.bin/<filename>`. Used as a fallback for
/// monorepos where the `node_modules` lives below the workspace
/// root (per-workspace install layout) rather than at or above
/// it (hoisted layout).
///
/// Skips `.git/` and any `node_modules/` directory we're not
/// directly probing — recursing into a `node_modules/` would
/// turn one missing binary into a `find / -name foo` storm. We
/// also don't traverse symlinks, same reason.
///
/// Returns the first match found in directory-iteration order.
/// In practice every match is the right one — a monorepo doesn't
/// install two oxlints at different versions across packages and
/// expect us to "pick the right one" without a separate signal.
fn scan_for_node_modules_bin(root: &Path, filename: &str, max_depth: u32) -> Option<PathBuf> {
	scan_for_node_modules_bin_inner(root, filename, max_depth, 0)
}

fn scan_for_node_modules_bin_inner(dir: &Path, filename: &str, max_depth: u32, depth: u32) -> Option<PathBuf> {
	// At every level the *current* dir gets a probe — covers the
	// case where `dir` itself has a `node_modules/.bin/<filename>`
	// the ancestor walk in `discover_binary` already missed
	// (shouldn't happen given `start.ancestors()` covers `start`
	// itself, but it's free and keeps the recursion shape uniform).
	let candidate = dir.join("node_modules").join(".bin").join(filename);
	if candidate.exists() {
		return Some(candidate);
	}
	if depth >= max_depth {
		return None;
	}
	let iter = std::fs::read_dir(dir).ok()?;
	for entry in iter.flatten() {
		let Ok(file_type) = entry.file_type() else {
			continue;
		};
		// `is_dir()` follows symlinks, which we don't want for
		// the recursive descent — `is_symlink()` short-circuits
		// the symlink case and `is_dir()` on the metadata
		// without traversal isn't directly available, so test
		// `is_symlink()` first.
		if file_type.is_symlink() || !file_type.is_dir() {
			continue;
		}
		let name = entry.file_name();
		// `node_modules/` itself is the leaf we *probe* for at
		// this level (above), not something we descend into:
		// recursing inside a `node_modules` is what the cost
		// guardrails are protecting against.
		if name == "node_modules" || name == ".git" {
			continue;
		}
		// Hidden directories (`.next/`, `.cache/`, …) likewise
		// shouldn't host a real package, and walking them is
		// pure overhead.
		if let Some(s) = name.to_str() {
			if s.starts_with('.') {
				continue;
			}
		}
		let child = entry.path();
		if let Some(found) = scan_for_node_modules_bin_inner(&child, filename, max_depth, depth + 1) {
			return Some(found);
		}
	}
	None
}

/// Per-platform `(subdir, filename)` inside `.venv/` that hosts an
/// installed CLI. Unix venvs use `bin/<name>` (no extension), Windows
/// venvs use `Scripts/<name>.exe`. Tracks Python's own venv module
/// convention; `uv` follows the same layout.
fn python_venv_layout(bin_name: &str) -> (&'static str, String) {
	if cfg!(windows) {
		("Scripts", format!("{bin_name}.exe"))
	} else {
		("bin", bin_name.to_owned())
	}
}

/// Resolve the in-container absolute path of `spec`'s binary for a
/// DockerExec broker. Container terminals / LSPs don't inherit a
/// login shell, so `$PATH` only covers what the image itself puts
/// there (moon-base's rustup + fnm + bun prefixes) — a Node-ecosystem
/// binary installed via `bun install` into
/// `node_modules/.bin/<bin>` won't be found by a plain basename
/// invocation.
///
/// For `NodeModules` we mirror the host-side ancestor walk against
/// the filesystem, then translate the resulting host path into the
/// container's view. The walk has to happen against host paths
/// because the actual `exists()` check is done on the host (the
/// bind-mount is the same bytes, so existence is equivalent —
/// we just can't `stat()` container paths directly from here).
///
/// Only the active folder's own `node_modules` + its descendants
/// within the mount are reachable from the container: a hoisted
/// monorepo that stores `node_modules` **above** the active folder
/// lives outside the bind-mount and returns `None`, in which case
/// the broker falls back to the host spawner for that server.
///
/// For `CargoHome` we return the basename — `moon-base` installs
/// `rust-analyzer` via `rustup component add` at
/// `$CARGO_HOME/bin/rust-analyzer`, which the image's `PATH` env
/// covers — and let the container's own resolution do the rest.
pub fn container_binary_path(spec: &LspBinarySpec, translator: &PathTranslator) -> Option<PathBuf> {
	let (host_root, server_root) = match translator {
		PathTranslator::HostMount { host_root, server_root } => (host_root.as_std_path(), server_root.as_std_path()),
		// Callers should only hit this for a DockerExec broker,
		// which always carries a HostMount translator. Keep the
		// guard so a wire-up bug surfaces as None (→ NotAvailable
		// pill) rather than a silent host-path leak.
		PathTranslator::Identity { .. } => return None,
	};

	match spec.discovery {
		DiscoveryStrategy::NodeModules => {
			// Container is always Linux (moon-base is debian) —
			// no `.cmd` suffix dance the host path needs.
			let filename = spec.bin_name;
			// Tier 1: ancestor walk for the hoisted-root layout.
			for ancestor in host_root.ancestors() {
				let candidate = ancestor.join("node_modules").join(".bin").join(filename);
				if !candidate.exists() {
					continue;
				}
				// Only accept matches that sit inside the bind
				// mount. Hoisted monorepos with `node_modules`
				// at a parent of the active folder aren't
				// reachable from inside the container; the
				// caller then falls back to the host spawner.
				let Ok(rel) = candidate.strip_prefix(host_root) else {
					tracing::debug!(
						bin = spec.bin_name,
						host_path = %candidate.display(),
						host_root = %host_root.display(),
						"lsp: node_modules match sits outside the bind mount, \
						 container can't reach it — falling back to host"
					);
					return None;
				};
				let path = server_root.join(rel);
				tracing::debug!(
					bin = spec.bin_name,
					path = %path.display(),
					"lsp: resolved via container-side node_modules"
				);
				return Some(path);
			}
			// Tier 2: nested-workspace layout (per-package
			// `node_modules`). Same bounded scan the host path
			// uses; matches always sit inside `host_root` here
			// because we only descend from the bind-mount root,
			// so the relativise step always succeeds.
			if let Some(host_match) = scan_for_node_modules_bin(host_root, filename, NODE_MODULES_SCAN_DEPTH) {
				if let Ok(rel) = host_match.strip_prefix(host_root) {
					let path = server_root.join(rel);
					tracing::debug!(
						bin = spec.bin_name,
						path = %path.display(),
						"lsp: resolved via container-side nested workspace node_modules"
					);
					return Some(path);
				}
			}
			// Tier 3 (TypeScript only): a project-local native
			// `tsc` from typescript@7+ — same version-gated
			// fallback the host path takes in
			// [`discover_server_binary`]. Only the mount root's
			// own `node_modules` and descendants are reachable
			// from the container, so no ancestor walk here (the
			// container is Linux — plain filename, no `.cmd`).
			if spec.language_id == TS_SERVER.language_id {
				let host_match =
					ts7_tsc_in(&host_root.join("node_modules"), "tsc").or_else(|| nested_ts7_tsc(host_root, "tsc"));
				if let Some(host_match) = host_match {
					if let Ok(rel) = host_match.strip_prefix(host_root) {
						let path = server_root.join(rel);
						tracing::debug!(
							path = %path.display(),
							"lsp: resolved via container-side typescript@7 tsc"
						);
						return Some(path);
					}
				}
			}
			tracing::debug!(
				bin = spec.bin_name,
				host_root = %host_root.display(),
				"lsp: no node_modules/.bin/<bin> found below the mount root"
			);
			None
		}
		DiscoveryStrategy::CargoHome => Some(PathBuf::from(spec.bin_name)),
		DiscoveryStrategy::GoBin => Some(PathBuf::from(spec.bin_name)),
		DiscoveryStrategy::PythonVenv => {
			// Container is always Linux — same `bin/<name>` layout
			// every Unix venv uses.
			let filename = spec.bin_name;
			for ancestor in host_root.ancestors() {
				let candidate = ancestor.join(".venv").join("bin").join(filename);
				if !candidate.exists() {
					continue;
				}
				let Ok(rel) = candidate.strip_prefix(host_root) else {
					tracing::debug!(
						bin = spec.bin_name,
						host_path = %candidate.display(),
						host_root = %host_root.display(),
						"lsp: .venv match sits outside the bind mount, \
						 container can't reach it — falling back to host"
					);
					return None;
				};
				let path = server_root.join(rel);
				tracing::debug!(
					bin = spec.bin_name,
					path = %path.display(),
					"lsp: resolved via container-side .venv"
				);
				return Some(path);
			}
			tracing::debug!(
				bin = spec.bin_name,
				host_root = %host_root.display(),
				"lsp: no .venv/bin/<bin> found below the mount root"
			);
			None
		}
	}
}

/// Probe `rustup which <tool>` and return its reported path. Returns
/// `None` when rustup isn't on PATH, when the requested tool isn't
/// installed in the active toolchain, or any other failure mode —
/// all of which should gracefully surface as "server unavailable"
/// rather than treated as an error.
fn rustup_which(tool: &str) -> Option<PathBuf> {
	let out = std::process::Command::new("rustup")
		.args(["which", tool])
		.output()
		.ok()?;
	if !out.status.success() {
		return None;
	}
	let raw = String::from_utf8(out.stdout).ok()?;
	let trimmed = raw.trim();
	if trimmed.is_empty() {
		return None;
	}
	Some(PathBuf::from(trimmed))
}

/// Is `path` a rustup proxy shim (i.e. `~/.cargo/bin/<tool>` that's
/// really a symlink to the `rustup` binary)? A shim spawns rustup
/// with `argv[0] == <tool>`, which demands that the tool is a known
/// component in the active toolchain — not a contract we can
/// satisfy just because the file exists.
///
/// Only symlinks can be detected cheaply this way. On Windows
/// rustup installs per-tool `.exe` shims that are separate binaries
/// and harder to identify without running them. The `rustup_which`
/// probe above handles the Windows case by giving us the real path
/// before we look in `cargo_bin_candidate` at all, so missing
/// Windows-shim detection here is a small residual risk at most.
fn is_rustup_shim(path: &Path) -> bool {
	match std::fs::read_link(path) {
		Ok(target) => target.file_name() == Some(std::ffi::OsStr::new("rustup")),
		Err(_) => false,
	}
}

/// Resolve `$GOBIN/<bin_name>`, falling back to
/// `$GOPATH/bin/<bin_name>`, with `$GOPATH` defaulting to
/// `$HOME/go` (the toolchain default since Go 1.8). Returns
/// `None` only when we can't build any candidate at all (no
/// `$GOBIN`, no `$GOPATH`, no `$HOME` / `$USERPROFILE`) — caller
/// still has the `$PATH` escape hatch after this.
fn go_bin_candidate(bin_name: &str) -> Option<PathBuf> {
	let filename = if cfg!(windows) {
		format!("{bin_name}.exe")
	} else {
		bin_name.to_owned()
	};
	if let Some(gobin) = std::env::var_os("GOBIN") {
		return Some(PathBuf::from(gobin).join(&filename));
	}
	let gopath = std::env::var_os("GOPATH")
		.map(PathBuf::from)
		.or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join("go")))
		.or_else(|| std::env::var_os("USERPROFILE").map(|h| PathBuf::from(h).join("go")))?;
	Some(gopath.join("bin").join(filename))
}

/// Resolve `$CARGO_HOME/bin/<bin_name>` with the `$HOME/.cargo/bin/`
/// fallback that rustup uses when `$CARGO_HOME` isn't set. Returns
/// `None` only when we can't build any candidate at all (no
/// `$CARGO_HOME`, no `$HOME` / `$USERPROFILE`) — caller still has
/// the `$PATH` escape hatch after this.
fn cargo_bin_candidate(bin_name: &str) -> Option<PathBuf> {
	let filename = if cfg!(windows) {
		format!("{bin_name}.exe")
	} else {
		bin_name.to_owned()
	};
	let base = std::env::var_os("CARGO_HOME")
		.map(PathBuf::from)
		.or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cargo")))
		.or_else(|| std::env::var_os("USERPROFILE").map(|h| PathBuf::from(h).join(".cargo")))?;
	Some(base.join("bin").join(filename))
}

fn path_to_file_uri(path: &Path) -> lt::Uri {
	// `url::Url::from_file_path` handles the OS-specific cases
	// (Windows drive letters, percent-escaping) correctly; we then
	// parse the result back into `lsp_types::Uri` which is a newtype
	// around `fluent_uri`. Both libraries accept the same string
	// form, so the round-trip is lossless.
	let url = url::Url::from_file_path(path).unwrap_or_else(|_| {
		tracing::warn!(path = %path.display(), "lsp: failed to build file:// URL, using empty");
		url::Url::parse("file:///").expect("static parse")
	});
	use std::str::FromStr;
	lt::Uri::from_str(url.as_str()).unwrap_or_else(|e| {
		tracing::warn!(path = %path.display(), error = %e, "lsp: failed to parse file URL as URI");
		lt::Uri::from_str("file:///").expect("static parse")
	})
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::fs;
	use std::sync::Mutex;

	/// Tests that mutate `$CARGO_HOME` serialise on this mutex.
	/// `cargo test` runs tests in parallel by default and process
	/// env is global, so without the lock the two
	/// `CARGO_HOME`-mutating tests below race and one sporadically
	/// sees the other's value.
	static CARGO_HOME_LOCK: Mutex<()> = Mutex::new(());

	/// Same rationale as `CARGO_HOME_LOCK`: serialise the tests
	/// that touch `$GOBIN` / `$GOPATH` so they don't race each
	/// other or any other env-mutating test in the module.
	static GO_ENV_LOCK: Mutex<()> = Mutex::new(());

	#[cfg(unix)]
	fn make_executable(path: &Path) {
		use std::os::unix::fs::PermissionsExt;
		let mut perms = fs::metadata(path).unwrap().permissions();
		perms.set_mode(0o755);
		fs::set_permissions(path, perms).unwrap();
	}

	#[cfg(not(unix))]
	fn make_executable(_: &Path) {}

	/// Binary nestled in the start directory itself resolves.
	#[test]
	fn discover_finds_binary_in_same_dir() {
		let tmp = tempfile::tempdir().unwrap();
		let bin_dir = tmp.path().join("node_modules").join(".bin");
		fs::create_dir_all(&bin_dir).unwrap();
		let bin_name = if cfg!(windows) { "my-lsp.cmd" } else { "my-lsp" };
		let bin_path = bin_dir.join(bin_name);
		fs::write(&bin_path, b"#!/bin/sh\n").unwrap();
		make_executable(&bin_path);

		let found = discover_binary("my-lsp", DiscoveryStrategy::NodeModules, tmp.path());
		assert_eq!(found.as_deref(), Some(bin_path.as_path()));
	}

	/// Binary in an ancestor directory resolves — mimics pnpm's
	/// hoisted monorepo layout where `node_modules` lives at the
	/// repo root, not the active subdirectory.
	#[test]
	fn discover_walks_up_to_ancestor_node_modules() {
		let tmp = tempfile::tempdir().unwrap();
		let bin_dir = tmp.path().join("node_modules").join(".bin");
		fs::create_dir_all(&bin_dir).unwrap();
		let bin_name = if cfg!(windows) { "my-lsp.cmd" } else { "my-lsp" };
		let bin_path = bin_dir.join(bin_name);
		fs::write(&bin_path, b"#!/bin/sh\n").unwrap();
		make_executable(&bin_path);

		// Start from a nested subfolder; discovery should walk up
		// to tmp and find the bin there.
		let nested = tmp.path().join("apps").join("web");
		fs::create_dir_all(&nested).unwrap();

		let found = discover_binary("my-lsp", DiscoveryStrategy::NodeModules, &nested);
		assert_eq!(found.as_deref(), Some(bin_path.as_path()));
	}

	/// pnpm / yarn workspace layout: each package installs into its
	/// own `node_modules/.bin/`, with no hoisted root copy. The
	/// ancestor walk (the hoisted-layout fast path) misses those, so
	/// discovery falls through to the bounded downward scan. Mimics
	/// `~/repo/apps/api/node_modules/.bin/oxlint` while the IDE's
	/// workspace root is `~/repo/`.
	#[test]
	fn discover_finds_binary_in_nested_workspace_node_modules() {
		let tmp = tempfile::tempdir().unwrap();
		let nested_bin_dir = tmp.path().join("apps").join("api").join("node_modules").join(".bin");
		fs::create_dir_all(&nested_bin_dir).unwrap();
		let bin_name = if cfg!(windows) { "my-lsp.cmd" } else { "my-lsp" };
		let bin_path = nested_bin_dir.join(bin_name);
		fs::write(&bin_path, b"#!/bin/sh\n").unwrap();
		make_executable(&bin_path);

		let found = discover_binary("my-lsp", DiscoveryStrategy::NodeModules, tmp.path());
		assert_eq!(
			found.as_deref(),
			Some(bin_path.as_path()),
			"per-workspace node_modules under the workspace root must be discoverable"
		);
	}

	/// Drop a fake `.bin/tsc` plus a `typescript/package.json` at
	/// `version` into `root/node_modules/`. Returns the tsc path.
	fn plant_tsc(root: &Path, version: &str) -> PathBuf {
		let node_modules = root.join("node_modules");
		let bin_dir = node_modules.join(".bin");
		fs::create_dir_all(&bin_dir).unwrap();
		let bin_name = if cfg!(windows) { "tsc.cmd" } else { "tsc" };
		let bin_path = bin_dir.join(bin_name);
		fs::write(&bin_path, b"#!/bin/sh\n").unwrap();
		make_executable(&bin_path);
		let pkg_dir = node_modules.join("typescript");
		fs::create_dir_all(&pkg_dir).unwrap();
		fs::write(
			pkg_dir.join("package.json"),
			format!("{{\"name\":\"typescript\",\"version\":\"{version}\"}}"),
		)
		.unwrap();
		bin_path
	}

	/// A project shipping `typescript@7` (native `tsc` with `--lsp`)
	/// and no `@typescript/native-preview` still resolves a TS LSP
	/// binary: the version-gated `tsc` tier.
	#[test]
	fn discover_ts7_tsc_accepts_typescript_7() {
		let tmp = tempfile::tempdir().unwrap();
		let tsc = plant_tsc(tmp.path(), "7.0.1");
		assert_eq!(discover_ts7_tsc(tmp.path()).as_deref(), Some(tsc.as_path()));
	}

	/// `typescript@6`'s `tsc` is the JS compiler with no `--lsp`
	/// mode — the version gate must reject it, otherwise the broker
	/// would spawn a child that never speaks LSP.
	#[test]
	fn discover_ts7_tsc_rejects_typescript_6() {
		let tmp = tempfile::tempdir().unwrap();
		plant_tsc(tmp.path(), "6.0.3");
		assert_eq!(discover_ts7_tsc(tmp.path()), None);
	}

	/// A `.bin/tsc` without a resolvable `typescript` package
	/// manifest (broken install, exotic layout) is rejected — no
	/// version proof, no spawn.
	#[test]
	fn discover_ts7_tsc_rejects_tsc_without_manifest() {
		let tmp = tempfile::tempdir().unwrap();
		let bin_dir = tmp.path().join("node_modules").join(".bin");
		fs::create_dir_all(&bin_dir).unwrap();
		let bin_name = if cfg!(windows) { "tsc.cmd" } else { "tsc" };
		fs::write(bin_dir.join(bin_name), b"#!/bin/sh\n").unwrap();
		assert_eq!(discover_ts7_tsc(tmp.path()), None);
	}

	/// Nested-workspace layout: `apps/api/node_modules/.bin/tsc`
	/// with typescript@7 resolves through the bounded scan tier.
	#[test]
	fn discover_ts7_tsc_finds_nested_workspace_install() {
		let tmp = tempfile::tempdir().unwrap();
		let pkg_root = tmp.path().join("apps").join("api");
		fs::create_dir_all(&pkg_root).unwrap();
		let tsc = plant_tsc(&pkg_root, "7.1.0");
		assert_eq!(discover_ts7_tsc(tmp.path()).as_deref(), Some(tsc.as_path()));
	}

	/// When both a project-local `tsgo` and a `typescript@7` `tsc`
	/// are installed, `tsgo` wins — it's the explicitly-installed
	/// preview channel and today's default.
	#[test]
	fn discover_server_binary_prefers_tsgo_over_ts7_tsc() {
		let tmp = tempfile::tempdir().unwrap();
		plant_tsc(tmp.path(), "7.0.1");
		let bin_dir = tmp.path().join("node_modules").join(".bin");
		let tsgo_name = if cfg!(windows) { "tsgo.cmd" } else { "tsgo" };
		let tsgo = bin_dir.join(tsgo_name);
		fs::write(&tsgo, b"#!/bin/sh\n").unwrap();
		make_executable(&tsgo);
		assert_eq!(
			discover_server_binary(&TS_SERVER, tmp.path()).as_deref(),
			Some(tsgo.as_path())
		);
	}

	/// Don't recurse into `node_modules/` when looking for a binary.
	/// A package that ships its own `node_modules/.bin/<name>`
	/// (vendored dep) shouldn't shadow the project's chosen one — and
	/// the cost of walking every nested `node_modules` would explode.
	#[test]
	fn discover_does_not_recurse_into_node_modules_for_nested_search() {
		let tmp = tempfile::tempdir().unwrap();
		// Bury a fake `my-lsp` deep inside a vendored package's own
		// `node_modules`. The scan must not find this — it's not the
		// project's chosen binary, and walking into nested
		// `node_modules` would be a perf disaster on real repos.
		let buried = tmp
			.path()
			.join("apps")
			.join("api")
			.join("node_modules")
			.join("some-pkg")
			.join("node_modules")
			.join(".bin");
		fs::create_dir_all(&buried).unwrap();
		let bin_name = if cfg!(windows) { "my-lsp.cmd" } else { "my-lsp" };
		let bin_path = buried.join(bin_name);
		fs::write(&bin_path, b"#!/bin/sh\n").unwrap();
		make_executable(&bin_path);

		let found = discover_binary("my-lsp", DiscoveryStrategy::NodeModules, tmp.path());
		assert!(
			found.is_none(),
			"nested-search must skip `node_modules/` itself; got {found:?}"
		);
	}

	/// A project-local copy beats whatever happens to be on PATH.
	/// Regression guard: if someone ever "optimises" discovery by
	/// checking PATH first, this test flips red.
	#[test]
	fn discover_prefers_project_local_over_path() {
		let tmp = tempfile::tempdir().unwrap();
		let bin_dir = tmp.path().join("node_modules").join(".bin");
		fs::create_dir_all(&bin_dir).unwrap();
		// Pick a binary every CI box has on PATH (sh/cmd.exe) so the
		// test's "beats PATH" assertion is actually observable.
		let (probe_name, probe_file) = if cfg!(windows) {
			("cmd", "cmd.cmd")
		} else {
			("sh", "sh")
		};
		let local = bin_dir.join(probe_file);
		fs::write(&local, b"#!/bin/sh\n").unwrap();
		make_executable(&local);

		let found =
			discover_binary(probe_name, DiscoveryStrategy::NodeModules, tmp.path()).expect("project-local should resolve");
		assert_eq!(found, local, "project-local copy must win over PATH");
	}

	/// Missing binary returns None rather than erroring — that's the
	/// contract the broker relies on to surface `NotAvailable`.
	#[test]
	fn discover_returns_none_when_missing_everywhere() {
		let tmp = tempfile::tempdir().unwrap();
		let found = discover_binary(
			"definitely-not-a-real-lsp-server-xyzzy",
			DiscoveryStrategy::NodeModules,
			tmp.path(),
		);
		assert!(found.is_none());
	}

	/// Rustup pre-creates a symlink at `$CARGO_HOME/bin/<tool>` for
	/// every known component, regardless of whether the component is
	/// actually installed. Running the shim when the component is
	/// missing fails with `Unknown binary`. Discovery must see
	/// through that and refuse the shim so the broker surfaces the
	/// right install hint.
	///
	/// Unix-only: on Windows rustup uses per-tool `.exe` shims that
	/// aren't cheaply distinguishable without running them. The
	/// `rustup_which` probe handles that case before we get here.
	#[cfg(unix)]
	#[test]
	fn discover_rejects_rustup_shim_in_cargo_home() {
		use std::os::unix::fs::symlink;

		let _guard = CARGO_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
		let tmp = tempfile::tempdir().unwrap();
		let bin_dir = tmp.path().join("bin");
		fs::create_dir_all(&bin_dir).unwrap();
		// Create a fake `rustup` binary first — it's the symlink
		// target we check for. Contents don't matter for the shim
		// test; we never spawn it here.
		let rustup_path = bin_dir.join("rustup");
		fs::write(&rustup_path, b"#!/bin/sh\nexit 101\n").unwrap();
		make_executable(&rustup_path);
		// Now drop a shim that points at it — exactly what rustup
		// does for known-but-not-installed components.
		let shim = bin_dir.join("phantom-tool");
		symlink("rustup", &shim).unwrap();

		let prev = std::env::var_os("CARGO_HOME");
		// SAFETY: single-threaded env mutation for this test; previous
		// value restored below. See `discover_uses_cargo_home_when_set`
		// for the rationale.
		unsafe {
			std::env::set_var("CARGO_HOME", tmp.path());
		}
		let found = discover_binary("phantom-tool", DiscoveryStrategy::CargoHome, tmp.path());
		// SAFETY: see above.
		unsafe {
			match prev {
				Some(v) => std::env::set_var("CARGO_HOME", v),
				None => std::env::remove_var("CARGO_HOME"),
			}
		}
		assert!(
			found.is_none(),
			"a rustup shim must not be reported as a usable LSP binary"
		);
	}

	/// CargoHome strategy resolves `$CARGO_HOME/bin/<bin>` when set.
	/// Test pattern: mutating process env is a global concern, so we
	/// set the var for just this test's duration (and pick a binary
	/// name nothing else would ever find, so leaked state never
	/// masks a real failure elsewhere).
	#[test]
	fn discover_uses_cargo_home_when_set() {
		let _guard = CARGO_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
		let tmp = tempfile::tempdir().unwrap();
		let bin_dir = tmp.path().join("bin");
		fs::create_dir_all(&bin_dir).unwrap();
		let bin_name = if cfg!(windows) { "fake-ra.exe" } else { "fake-ra" };
		let bin_path = bin_dir.join(bin_name);
		fs::write(&bin_path, b"#!/bin/sh\n").unwrap();
		make_executable(&bin_path);

		// `CARGO_HOME` points at the temp dir; the file under
		// `$CARGO_HOME/bin/` must win over any PATH lookup that
		// might find a real rust-analyzer on the dev's machine.
		//
		// SAFETY: `std::env::set_var` was marked unsafe in Rust
		// 1.90 because multi-threaded writes to the env table can
		// race with libc readers. This test binary is
		// single-threaded for this read/write, and we restore the
		// original value on exit; no other test in this module
		// touches `CARGO_HOME`. Accepted trade-off for the
		// coverage.
		let prev = std::env::var_os("CARGO_HOME");
		// SAFETY: see above.
		unsafe {
			std::env::set_var("CARGO_HOME", tmp.path());
		}
		let found = discover_binary("fake-ra", DiscoveryStrategy::CargoHome, tmp.path());
		// SAFETY: see above.
		unsafe {
			match prev {
				Some(v) => std::env::set_var("CARGO_HOME", v),
				None => std::env::remove_var("CARGO_HOME"),
			}
		}
		assert_eq!(found.as_deref(), Some(bin_path.as_path()));
	}

	/// `Identity` translator is a plain join / strip. Round-tripping
	/// a workspace-relative path through absolutise → relativise
	/// must come back identical or the LSP layer silently loses
	/// track of which buffer a diagnostic belongs to.
	#[test]
	fn translator_identity_round_trip() {
		let t = PathTranslator::Identity {
			host_root: Utf8PathBuf::from("/home/dev/code/moon-ide"),
		};
		let abs = t.absolutise("src/lib/state.svelte.ts");
		assert_eq!(
			abs,
			Utf8PathBuf::from("/home/dev/code/moon-ide/src/lib/state.svelte.ts")
		);
		let rel = t.relativise(abs.as_std_path()).expect("round-trip");
		assert_eq!(rel, "src/lib/state.svelte.ts");
		assert_eq!(t.host_root(), t.server_root());
	}

	/// `HostMount` round-trip: what the server sees is
	/// `/workspace/<basename>/...`, what the tree and the frontend
	/// see is the host absolute path, and the forward-slash
	/// workspace-relative string must be identical across both
	/// views.
	#[test]
	fn translator_host_mount_round_trip() {
		let t = PathTranslator::HostMount {
			host_root: Utf8PathBuf::from("/home/dev/code/moon-ide"),
			server_root: Utf8PathBuf::from("/workspace/moon-ide"),
		};
		let abs = t.absolutise("crates/moon-core/src/lsp/server.rs");
		assert_eq!(
			abs,
			Utf8PathBuf::from("/workspace/moon-ide/crates/moon-core/src/lsp/server.rs")
		);
		let rel = t.relativise(abs.as_std_path()).expect("round-trip");
		assert_eq!(rel, "crates/moon-core/src/lsp/server.rs");
		// Callers that need tree state use host_root, not the
		// containerised server_root.
		assert_eq!(t.host_root(), &Utf8PathBuf::from("/home/dev/code/moon-ide"));
		assert_eq!(t.server_root(), &Utf8PathBuf::from("/workspace/moon-ide"));
	}

	/// Diagnostics against files outside the server's view (e.g.
	/// rust-analyzer publishing against `/usr/local/.../core.rs`
	/// inside the container) are dropped silently — the UI has no
	/// buffer for them. Regression guard for the `strip_prefix`
	/// guard in `relativise`.
	#[test]
	fn translator_relativise_rejects_paths_outside_root() {
		let t = PathTranslator::HostMount {
			host_root: Utf8PathBuf::from("/home/dev/code/moon-ide"),
			server_root: Utf8PathBuf::from("/workspace/moon-ide"),
		};
		let outside = Path::new("/usr/local/lib/rustlib/src/rust/library/core/src/lib.rs");
		assert!(t.relativise(outside).is_none());
	}

	/// Regression for the original user bug: a container-backed
	/// broker was handing `docker exec` a bare `tsgo` basename,
	/// which isn't on the container's `$PATH`. The fix resolves
	/// the binary to the in-container `node_modules/.bin/<bin>`
	/// path built from the host's ancestor walk.
	#[test]
	fn container_binary_path_resolves_node_modules_inside_mount() {
		let tmp = tempfile::tempdir().unwrap();
		let host_root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
		let bin_dir = host_root.join("node_modules").join(".bin");
		fs::create_dir_all(bin_dir.as_std_path()).unwrap();
		let bin_path = bin_dir.join("tsgo");
		fs::write(bin_path.as_std_path(), b"#!/usr/bin/env node\n").unwrap();
		make_executable(bin_path.as_std_path());

		let translator = PathTranslator::HostMount {
			host_root: host_root.clone(),
			server_root: Utf8PathBuf::from("/workspace/moon-ide"),
		};
		let resolved = container_binary_path(&TS_SERVER, &translator).expect("resolves when node_modules is in the mount");
		assert_eq!(resolved, Path::new("/workspace/moon-ide/node_modules/.bin/tsgo"));
	}

	/// pnpm-hoisted monorepos keep `node_modules` at a parent of
	/// the active folder. That parent isn't bind-mounted, so the
	/// container can't see the binary. `container_binary_path`
	/// must say so cleanly — `None` — rather than handing back a
	/// mount-escaping server path. The broker then cascades to
	/// the host fallback.
	#[test]
	fn container_binary_path_rejects_node_modules_above_mount() {
		let tmp = tempfile::tempdir().unwrap();
		let monorepo = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
		let bin_dir = monorepo.join("node_modules").join(".bin");
		fs::create_dir_all(bin_dir.as_std_path()).unwrap();
		let bin_path = bin_dir.join("tsgo");
		fs::write(bin_path.as_std_path(), b"#!/usr/bin/env node\n").unwrap();
		make_executable(bin_path.as_std_path());

		let active = monorepo.join("packages").join("app");
		fs::create_dir_all(active.as_std_path()).unwrap();

		let translator = PathTranslator::HostMount {
			host_root: active,
			server_root: Utf8PathBuf::from("/workspace/app"),
		};
		assert!(
			container_binary_path(&TS_SERVER, &translator).is_none(),
			"hoisted node_modules sits above the bind mount — container can't reach it"
		);
	}

	/// `CargoHome`-strategy specs return the basename: `moon-base`
	/// installs `rust-analyzer` on the container's `$PATH` via
	/// rustup, so `docker exec` can resolve it without an
	/// absolute path.
	#[test]
	fn container_binary_path_cargo_home_returns_basename() {
		let translator = PathTranslator::HostMount {
			host_root: Utf8PathBuf::from("/home/dev/code/moon-ide"),
			server_root: Utf8PathBuf::from("/workspace/moon-ide"),
		};
		let resolved =
			container_binary_path(&RUST_SERVER, &translator).expect("rust-analyzer always returns Some for CargoHome");
		assert_eq!(resolved, Path::new("rust-analyzer"));
	}

	/// Project-local `.venv/bin/ty` resolves the same way
	/// `node_modules/.bin/tsgo` does: walk ancestors, prefer a
	/// project-local hit over `$PATH`. Mirrors the TS regression
	/// test above.
	#[test]
	fn discover_python_venv_finds_binary_in_same_dir() {
		let tmp = tempfile::tempdir().unwrap();
		let (subdir, filename) = if cfg!(windows) {
			("Scripts", "ty.exe")
		} else {
			("bin", "ty")
		};
		let bin_dir = tmp.path().join(".venv").join(subdir);
		fs::create_dir_all(&bin_dir).unwrap();
		let bin_path = bin_dir.join(filename);
		fs::write(&bin_path, b"#!/bin/sh\n").unwrap();
		make_executable(&bin_path);

		let found = discover_binary("ty", DiscoveryStrategy::PythonVenv, tmp.path());
		assert_eq!(found.as_deref(), Some(bin_path.as_path()));
	}

	/// uv's `workspace` layout puts `.venv` at the repo root and
	/// individual packages a level or two below. The walk should
	/// climb out of a nested package and find the parent venv.
	#[test]
	fn discover_python_venv_walks_up_to_ancestor_venv() {
		let tmp = tempfile::tempdir().unwrap();
		let (subdir, filename) = if cfg!(windows) {
			("Scripts", "ty.exe")
		} else {
			("bin", "ty")
		};
		let bin_dir = tmp.path().join(".venv").join(subdir);
		fs::create_dir_all(&bin_dir).unwrap();
		let bin_path = bin_dir.join(filename);
		fs::write(&bin_path, b"#!/bin/sh\n").unwrap();
		make_executable(&bin_path);

		let nested = tmp.path().join("packages").join("api");
		fs::create_dir_all(&nested).unwrap();

		let found = discover_binary("ty", DiscoveryStrategy::PythonVenv, &nested);
		assert_eq!(found.as_deref(), Some(bin_path.as_path()));
	}

	/// A `.venv/` inside the bind mount translates through to its
	/// container path the same way `node_modules/.bin/` does.
	#[test]
	fn container_binary_path_resolves_python_venv_inside_mount() {
		let tmp = tempfile::tempdir().unwrap();
		let host_root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
		let bin_dir = host_root.join(".venv").join("bin");
		fs::create_dir_all(bin_dir.as_std_path()).unwrap();
		let bin_path = bin_dir.join("ty");
		fs::write(bin_path.as_std_path(), b"#!/usr/bin/env python\n").unwrap();
		make_executable(bin_path.as_std_path());

		let translator = PathTranslator::HostMount {
			host_root: host_root.clone(),
			server_root: Utf8PathBuf::from("/workspace/moon-py"),
		};
		let resolved = container_binary_path(&PYTHON_SERVER, &translator).expect("resolves when .venv is in the mount");
		assert_eq!(resolved, Path::new("/workspace/moon-py/.venv/bin/ty"));
	}

	/// `GoBin`-strategy specs return the basename: `moon-base`
	/// installs `gopls` on the container's `$PATH` via
	/// `go install`, so `docker exec` can resolve it without
	/// an absolute path. Mirrors the CargoHome shape.
	#[test]
	fn container_binary_path_go_bin_returns_basename() {
		let translator = PathTranslator::HostMount {
			host_root: Utf8PathBuf::from("/home/dev/code/gitaly"),
			server_root: Utf8PathBuf::from("/workspace/gitaly"),
		};
		let resolved = container_binary_path(&GO_SERVER, &translator).expect("gopls always returns Some for GoBin");
		assert_eq!(resolved, Path::new("gopls"));
	}

	/// `$GOBIN` points at an explicit override directory; the
	/// candidate must live there even when `$GOPATH` is set
	/// elsewhere. The Go toolchain itself prefers `$GOBIN` over
	/// `$GOPATH/bin` and discovery should match.
	#[test]
	fn discover_uses_gobin_when_set() {
		let _guard = GO_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
		let tmp = tempfile::tempdir().unwrap();
		let bin_dir = tmp.path().join("explicit-gobin");
		fs::create_dir_all(&bin_dir).unwrap();
		let bin_name = if cfg!(windows) { "fake-gopls.exe" } else { "fake-gopls" };
		let bin_path = bin_dir.join(bin_name);
		fs::write(&bin_path, b"#!/bin/sh\n").unwrap();
		make_executable(&bin_path);

		let prev_gobin = std::env::var_os("GOBIN");
		let prev_gopath = std::env::var_os("GOPATH");
		// SAFETY: see CARGO_HOME-mutation rationale on
		// `discover_uses_cargo_home_when_set`. Single-threaded
		// env mutation, restored on exit, GO_ENV_LOCK serialises
		// every other test in this module that touches the same
		// vars.
		unsafe {
			std::env::set_var("GOBIN", &bin_dir);
			std::env::remove_var("GOPATH");
		}
		let found = discover_binary("fake-gopls", DiscoveryStrategy::GoBin, tmp.path());
		// SAFETY: see above.
		unsafe {
			match prev_gobin {
				Some(v) => std::env::set_var("GOBIN", v),
				None => std::env::remove_var("GOBIN"),
			}
			match prev_gopath {
				Some(v) => std::env::set_var("GOPATH", v),
				None => std::env::remove_var("GOPATH"),
			}
		}
		assert_eq!(found.as_deref(), Some(bin_path.as_path()));
	}

	/// Without `$GOBIN`, discovery falls through to
	/// `$GOPATH/bin/<bin>`. Regression guard for the dual-env
	/// resolution shape.
	#[test]
	fn discover_uses_gopath_bin_when_gobin_unset() {
		let _guard = GO_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
		let tmp = tempfile::tempdir().unwrap();
		let bin_dir = tmp.path().join("bin");
		fs::create_dir_all(&bin_dir).unwrap();
		let bin_name = if cfg!(windows) { "fake-gopls.exe" } else { "fake-gopls" };
		let bin_path = bin_dir.join(bin_name);
		fs::write(&bin_path, b"#!/bin/sh\n").unwrap();
		make_executable(&bin_path);

		let prev_gobin = std::env::var_os("GOBIN");
		let prev_gopath = std::env::var_os("GOPATH");
		// SAFETY: see above.
		unsafe {
			std::env::remove_var("GOBIN");
			std::env::set_var("GOPATH", tmp.path());
		}
		let found = discover_binary("fake-gopls", DiscoveryStrategy::GoBin, tmp.path());
		// SAFETY: see above.
		unsafe {
			match prev_gobin {
				Some(v) => std::env::set_var("GOBIN", v),
				None => std::env::remove_var("GOBIN"),
			}
			match prev_gopath {
				Some(v) => std::env::set_var("GOPATH", v),
				None => std::env::remove_var("GOPATH"),
			}
		}
		assert_eq!(found.as_deref(), Some(bin_path.as_path()));
	}

	/// A venv at a parent of the active folder isn't visible from
	/// inside the container — same monorepo escape logic as for
	/// `node_modules`.
	#[test]
	fn container_binary_path_rejects_python_venv_above_mount() {
		let tmp = tempfile::tempdir().unwrap();
		let monorepo = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
		let bin_dir = monorepo.join(".venv").join("bin");
		fs::create_dir_all(bin_dir.as_std_path()).unwrap();
		let bin_path = bin_dir.join("ty");
		fs::write(bin_path.as_std_path(), b"#!/usr/bin/env python\n").unwrap();
		make_executable(bin_path.as_std_path());

		let active = monorepo.join("packages").join("api");
		fs::create_dir_all(active.as_std_path()).unwrap();

		let translator = PathTranslator::HostMount {
			host_root: active,
			server_root: Utf8PathBuf::from("/workspace/api"),
		};
		assert!(
			container_binary_path(&PYTHON_SERVER, &translator).is_none(),
			"hoisted .venv sits above the bind mount — container can't reach it"
		);
	}

	/// `parse_watched_files_registration` round-trips a typical
	/// tsserver-shaped registration (string globs, default kind)
	/// into a usable matcher list.
	#[test]
	fn watched_files_registration_compiles_string_globs() {
		let reg = lt::Registration {
			id: "ts-files".to_owned(),
			method: "workspace/didChangeWatchedFiles".to_owned(),
			register_options: Some(serde_json::json!({
				"watchers": [
					{ "globPattern": "**/*.ts" },
					{ "globPattern": "**/tsconfig*.json", "kind": 2 }
				]
			})),
		};
		let (id, patterns) = parse_watched_files_registration(reg).expect("registration parses");
		assert_eq!(id, "ts-files");
		assert_eq!(patterns.len(), 2);
		assert!(patterns[0].matcher.is_match("src/main.ts"));
		assert!(patterns[0].matcher.is_match("nested/foo/bar.ts"));
		assert!(!patterns[0].matcher.is_match("src/main.rs"));
		// Default kind defaults to 7 (all three) when omitted.
		assert!(
			patterns[0].kind.contains(lt::WatchKind::Change),
			"default kind must include Change so notify_files_changed actually fires"
		);
		// Explicit kind=2 is preserved.
		assert_eq!(patterns[1].kind.bits(), 2);
		assert!(patterns[1].matcher.is_match("tsconfig.json"));
		assert!(patterns[1].matcher.is_match("packages/api/tsconfig.build.json"));
	}

	/// Empty `watchers` list → `None` so the caller treats it
	/// as "no registration to record". Same end-state as a
	/// server that never sent the registration at all.
	#[test]
	fn watched_files_registration_empty_returns_none() {
		let reg = lt::Registration {
			id: "noop".to_owned(),
			method: "workspace/didChangeWatchedFiles".to_owned(),
			register_options: Some(serde_json::json!({ "watchers": [] })),
		};
		assert!(parse_watched_files_registration(reg).is_none());
	}

	/// Missing `register_options` entirely → `None`. Servers
	/// shouldn't send this for `didChangeWatchedFiles`, but the
	/// LSP type allows it as `Option<Value>` and we don't crash
	/// the pump on a malformed payload.
	#[test]
	fn watched_files_registration_missing_options_returns_none() {
		let reg = lt::Registration {
			id: "broken".to_owned(),
			method: "workspace/didChangeWatchedFiles".to_owned(),
			register_options: None,
		};
		assert!(parse_watched_files_registration(reg).is_none());
	}

	/// A bad glob pattern is logged + skipped; the rest of the
	/// list still compiles. Reflects real-world server bugs
	/// where one pattern is malformed but the others are fine.
	#[test]
	fn watched_files_registration_skips_bad_globs() {
		let reg = lt::Registration {
			id: "mixed".to_owned(),
			method: "workspace/didChangeWatchedFiles".to_owned(),
			register_options: Some(serde_json::json!({
				"watchers": [
					{ "globPattern": "**/*.rs" },
					{ "globPattern": "[" }
				]
			})),
		};
		let (_, patterns) = parse_watched_files_registration(reg).expect("at least one valid pattern");
		assert_eq!(patterns.len(), 1, "the bad glob is dropped, the good one survives");
		assert!(patterns[0].matcher.is_match("crates/foo/src/lib.rs"));
	}

	/// `Relative` glob patterns flatten to their inner pattern
	/// string. The spec scopes them to a `WorkspaceFolder`; we
	/// open one folder per broker so the distinction collapses.
	#[test]
	fn watched_files_registration_handles_relative_pattern() {
		let reg = lt::Registration {
			id: "relative".to_owned(),
			method: "workspace/didChangeWatchedFiles".to_owned(),
			register_options: Some(serde_json::json!({
				"watchers": [
					{
						"globPattern": {
							"baseUri": "file:///workspace",
							"pattern": "**/*.go"
						}
					}
				]
			})),
		};
		let (_, patterns) = parse_watched_files_registration(reg).expect("relative pattern parses");
		assert!(patterns[0].matcher.is_match("cmd/main.go"));
		assert!(!patterns[0].matcher.is_match("README.md"));
	}

	/// The svelte server follows the same lockfile-aware hint shape
	/// as the other npm-distributed servers.
	#[test]
	fn install_hint_for_svelte_follows_lockfile() {
		let tmp = tempfile::tempdir().unwrap();
		let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
		assert_eq!(
			resolve_install_hint(&SVELTE_SERVER, &root),
			"bun add -D svelte-language-server"
		);
		fs::write(root.join("pnpm-lock.yaml"), "lockfileVersion: 6\n").unwrap();
		assert_eq!(
			resolve_install_hint(&SVELTE_SERVER, &root),
			"pnpm -wD add svelte-language-server"
		);
	}

	/// `pnpm-lock.yaml` at the workspace root flips the TS install
	/// hint to the pnpm form so the pill tooltip is copy-pasteable
	/// in pnpm-managed monorepos.
	#[test]
	fn install_hint_picks_pnpm_for_pnpm_lock() {
		let tmp = tempfile::tempdir().unwrap();
		let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
		fs::write(root.join("pnpm-lock.yaml"), "lockfileVersion: 6\n").unwrap();
		assert_eq!(
			resolve_install_hint(&TS_SERVER, &root),
			"pnpm -wD add @typescript/native-preview"
		);
	}

	/// `package-lock.json` at the workspace root flips the TS hint
	/// to `npm i -D`. A repo without a lockfile (or with `bun.lock`)
	/// keeps the static `bun add -D` default.
	#[test]
	fn install_hint_picks_npm_for_npm_lock() {
		let tmp = tempfile::tempdir().unwrap();
		let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
		fs::write(root.join("package-lock.json"), "{}").unwrap();
		assert_eq!(
			resolve_install_hint(&TS_SERVER, &root),
			"npm i -D @typescript/native-preview"
		);
	}

	/// `pnpm-lock.yaml` wins over `package-lock.json` when both are
	/// present (which happens during a manager migration). Pinning
	/// the priority avoids the hint flickering between forms based
	/// on directory iteration order.
	#[test]
	fn install_hint_prefers_pnpm_over_npm() {
		let tmp = tempfile::tempdir().unwrap();
		let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
		fs::write(root.join("pnpm-lock.yaml"), "lockfileVersion: 6\n").unwrap();
		fs::write(root.join("package-lock.json"), "{}").unwrap();
		assert_eq!(
			resolve_install_hint(&TS_SERVER, &root),
			"pnpm -wD add @typescript/native-preview"
		);
	}

	/// No lockfile (or only `bun.lock`) keeps the static default —
	/// matches moon-ide itself and is the safe fallback.
	#[test]
	fn install_hint_defaults_to_bun() {
		let tmp = tempfile::tempdir().unwrap();
		let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
		assert_eq!(
			resolve_install_hint(&TS_SERVER, &root),
			"bun add -D @typescript/native-preview"
		);
		fs::write(root.join("bun.lock"), "").unwrap();
		assert_eq!(
			resolve_install_hint(&TS_SERVER, &root),
			"bun add -D @typescript/native-preview"
		);
	}

	/// Non-TypeScript specs are unaffected — they all have one
	/// canonical install path each.
	#[test]
	fn install_hint_passthrough_for_other_languages() {
		let tmp = tempfile::tempdir().unwrap();
		let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
		fs::write(root.join("pnpm-lock.yaml"), "lockfileVersion: 6\n").unwrap();
		assert_eq!(
			resolve_install_hint(&RUST_SERVER, &root),
			"rustup component add rust-analyzer"
		);
		assert_eq!(
			resolve_install_hint(&GO_SERVER, &root),
			"go install golang.org/x/tools/gopls@latest"
		);
		assert_eq!(
			resolve_install_hint(&PYTHON_SERVER, &root),
			"uv add --dev ty (or uv tool install ty)"
		);
	}

	/// Oxlint's install hint follows the same lockfile-driven
	/// shape as the TS server: pnpm / npm-aware where the user's
	/// project pins a manager, otherwise the static `bun add -D`
	/// default.
	#[test]
	fn install_hint_for_oxlint_follows_lockfile() {
		let tmp = tempfile::tempdir().unwrap();
		let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
		assert_eq!(resolve_install_hint(&OXLINT_LINTER, &root), "bun add -D oxlint");

		fs::write(root.join("pnpm-lock.yaml"), "lockfileVersion: 6\n").unwrap();
		assert_eq!(resolve_install_hint(&OXLINT_LINTER, &root), "pnpm -wD add oxlint");

		fs::remove_file(root.join("pnpm-lock.yaml")).unwrap();
		fs::write(root.join("package-lock.json"), "{}").unwrap();
		assert_eq!(resolve_install_hint(&OXLINT_LINTER, &root), "npm i -D oxlint");
	}

	/// `OXLINT_LANGUAGES` covers exactly the file language ids
	/// `tsgo` covers — they're co-tenants on every JS/TS file, so
	/// the sets must agree. Drift here means oxlint silently
	/// skips some files that otherwise have a TS server running.
	#[test]
	fn oxlint_covers_same_languages_as_ts_server() {
		for lang in ["typescript", "typescriptreact", "javascript", "javascriptreact"] {
			assert!(OXLINT_LANGUAGES.contains(&lang), "oxlint should cover {lang}");
		}
		assert!(!OXLINT_LANGUAGES.contains(&"rust"));
		assert!(!OXLINT_LANGUAGES.contains(&"python"));
	}

	/// The empty-tree case: no nested `.oxlintrc.json`, just the
	/// root entry. Needed because the broker hands the result
	/// straight to `LspServer::spawn` and an empty list there
	/// would surface as "no workspace folders at all" — some
	/// servers treat that as "work file-by-file, ignore configs".
	#[test]
	fn discover_oxlint_workspace_folders_returns_root_when_no_configs() {
		let tmp = tempfile::tempdir().unwrap();
		let root = tmp.path();
		let folders = discover_oxlint_workspace_folders(root);
		assert_eq!(folders, vec![String::new()]);
	}

	/// Real monorepo shape: `.oxlintrc.json` at root + nested
	/// packages. We expect the root entry (`""`) plus each
	/// nested directory, sorted, no duplicates. A root-level
	/// `.oxlintrc.json` should NOT add a separate entry — it's
	/// already covered by the empty string.
	#[test]
	fn discover_oxlint_workspace_folders_walks_nested_packages() {
		let tmp = tempfile::tempdir().unwrap();
		let root = tmp.path();
		fs::write(root.join(".oxlintrc.json"), "{}").unwrap();
		fs::create_dir_all(root.join("apps/api")).unwrap();
		fs::write(root.join("apps/api/.oxlintrc.json"), "{}").unwrap();
		fs::create_dir_all(root.join("apps/game-server")).unwrap();
		fs::write(root.join("apps/game-server/.oxlintrc.json"), "{}").unwrap();
		fs::create_dir_all(root.join("packages/utils")).unwrap();
		fs::write(root.join("packages/utils/.oxlintrc.json"), "{}").unwrap();

		let mut folders = discover_oxlint_workspace_folders(root);
		folders.sort();
		assert_eq!(
			folders,
			vec![
				"".to_owned(),
				"apps/api".to_owned(),
				"apps/game-server".to_owned(),
				"packages/utils".to_owned(),
			]
		);
	}

	/// `node_modules/` is `.gitignore`'d by default in any real
	/// project (and our `WalkBuilder` honours it). Confirm a
	/// vendored `.oxlintrc.json` from a dependency doesn't show
	/// up as its own workspace folder — that would point oxlint
	/// at someone else's config and misreport diagnostics.
	/// `node_modules/` is ignored in any real project (`.gitignore`
	/// inside a git repo, or a top-level `.ignore` file the walker
	/// also honours). We use `.ignore` here because tempdirs aren't
	/// git repos and `WalkBuilder::git_ignore` only kicks in when
	/// it can find a `.git/` ancestor — `.ignore` works
	/// unconditionally and is the convention `ripgrep` and friends
	/// document for ignored-but-not-VCS-ignored layouts. The real
	/// codebase always has `.gitignore`, so the assertion still
	/// proves the intended behaviour: a vendored `.oxlintrc.json`
	/// from a dependency must not show up as its own workspace
	/// folder, otherwise oxlint would lint half the project with
	/// somebody else's config.
	#[test]
	fn discover_oxlint_workspace_folders_skips_node_modules() {
		let tmp = tempfile::tempdir().unwrap();
		let root = tmp.path();
		fs::write(root.join(".ignore"), "node_modules\n").unwrap();
		fs::create_dir_all(root.join("node_modules/oxlint")).unwrap();
		fs::write(root.join("node_modules/oxlint/.oxlintrc.json"), "{}").unwrap();

		let folders = discover_oxlint_workspace_folders(root);
		assert_eq!(folders, vec![String::new()]);
	}
}
