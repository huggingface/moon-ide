//! Forge remote detection (GitHub / Forgejo) and the Forgejo REST
//! client.
//!
//! GitHub flows shell out to `gh` (JSON output, `gh api`, keyring
//! auth). Forgejo's `fj` CLI has neither machine-readable output nor
//! an API passthrough, so everything beyond `fj pr checkout` talks
//! straight to the instance's `/api/v1` using the token `fj` already
//! stores in its `keys.json` — we never prompt for or store a token
//! ourselves, and a `401` triggers one best-effort `fj whoami` run so
//! fj refreshes an expired OAuth login before we retry. See
//! ADR 0073 (specs/decisions/0073-forgejo-support.md).
//!
//! Which hosts count as Forgejo: `codeberg.org` is hardcoded (the
//! flagship instance), and any host the user has logged into with
//! `fj auth login` (i.e. present in `keys.json`) is trusted to be a
//! Forgejo instance. Everything else is an unsupported remote — we'd
//! rather leave a link un-rendered than guess a URL convention.

use std::collections::BTreeSet;

use camino::{Utf8Path, Utf8PathBuf};

/// Which forge a recognised remote belongs to. Drives URL shapes
/// (GitHub `/blob/…` vs Forgejo `/src/commit/…`), the PR CLI
/// (`gh` vs `fj`), and the PR-data path (gh JSON vs Forgejo REST).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForgeKind {
	GitHub,
	Forgejo,
}

/// A recognised `origin`/`upstream` remote, normalised to its web
/// identity. `web_base` is the canonical repo page
/// (`https://<host>/<owner_repo>`, no trailing slash, no `.git`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForgeRemote {
	pub kind: ForgeKind,
	/// Host (plus `:port` when the HTTPS remote carried one) —
	/// the API endpoint base for Forgejo instances.
	pub host: String,
	/// `owner/repo` path, `.git`-trimmed. May carry more segments
	/// on exotic setups; treated as an opaque path suffix.
	pub owner_repo: String,
	pub web_base: String,
}

impl ForgeRemote {
	/// Forgejo REST base (`https://<host>/api/v1`). Only meaningful
	/// for `ForgeKind::Forgejo`.
	fn api_base(&self) -> String {
		format!("https://{}/api/v1", self.host)
	}
}

/// Resolve the active folder's `origin` (then `upstream`) remote to
/// a recognised forge. `None` when neither remote maps to a known
/// host — every caller treats that as "no web links, no PR data".
pub fn detect_forge_remote(root: &Utf8Path) -> Option<ForgeRemote> {
	let forgejo_hosts = fj_known_hosts();
	for candidate in ["origin", "upstream"] {
		if let Some(url) = git_config_remote_url(root, candidate) {
			if let Some(remote) = normalize_remote_url_with(&url, &forgejo_hosts) {
				return Some(remote);
			}
			// Remote exists but isn't a supported host; keep looking
			// — the repo may have a recognised upstream behind a
			// custom origin.
		}
	}
	None
}

/// Web base URL of the recognised remote, or `None`. Thin wrapper
/// for the many callers that only need the URL string (blame links,
/// `#123` autolinks) and don't care which forge it is.
pub fn remote_web_url(root: &Utf8Path) -> Option<String> {
	detect_forge_remote(root).map(|remote| remote.web_base)
}

/// URL-normalising half of [`detect_forge_remote`], parameterised on
/// the known-Forgejo host set for unit tests. Returns `None` for any
/// URL we can't confidently map to a web base.
fn normalize_remote_url_with(raw: &str, forgejo_hosts: &BTreeSet<String>) -> Option<ForgeRemote> {
	// `git@host:owner/repo(.git)?` — SCP-style SSH.
	if let Some(rest) = raw.strip_prefix("git@") {
		if let Some((host, path)) = rest.split_once(':') {
			return classify(host, path, forgejo_hosts);
		}
	}
	// `ssh://git@host[:port]/owner/repo(.git)?`. The SSH port is not
	// the web port, so it's dropped from the web identity.
	if let Some(rest) = raw.strip_prefix("ssh://") {
		let rest = rest.strip_prefix("git@").unwrap_or(rest);
		if let Some((authority, path)) = rest.split_once('/') {
			let host = authority.split(':').next().unwrap_or(authority);
			return classify(host, path, forgejo_hosts);
		}
	}
	// `https://host[:port]/owner/repo(.git)?` — already the web
	// identity, port and all.
	if let Some(rest) = raw.strip_prefix("https://").or_else(|| raw.strip_prefix("http://")) {
		if let Some((host, path)) = rest.split_once('/') {
			return classify(host, path, forgejo_hosts);
		}
	}
	None
}

/// Map a parsed `(host, path)` pair to a [`ForgeRemote`], or `None`
/// for hosts we don't recognise. Port suffixes are ignored for the
/// *match* (fj records hosts without the SSH port) but kept in the
/// web identity for HTTPS remotes that carried one.
fn classify(host: &str, path: &str, forgejo_hosts: &BTreeSet<String>) -> Option<ForgeRemote> {
	if host.is_empty() || path.is_empty() {
		return None;
	}
	let bare_host = host.split(':').next().unwrap_or(host);
	let kind = if bare_host == "github.com" {
		ForgeKind::GitHub
	} else if bare_host == "codeberg.org" || forgejo_hosts.contains(bare_host) {
		ForgeKind::Forgejo
	} else {
		return None;
	};
	let owner_repo = path.trim_end_matches('/').trim_end_matches(".git");
	Some(ForgeRemote {
		kind,
		host: host.to_owned(),
		owner_repo: owner_repo.to_owned(),
		web_base: format!("https://{host}/{owner_repo}"),
	})
}

/// Read `remote.<name>.url` straight from the repo's config file,
/// **without invoking git**: the URL is plain file data, and the
/// host's git binary can be older than the repo. Concretely, the dev
/// container's git ≥ 2.48 writes `extensions.relativeWorktrees` into
/// the parent repo config when a worker worktree is created, after
/// which a 2.43 host git refuses to open the repo at all — even for
/// `git config --get`. Handles the worktree case too: a `.git`
/// *file* is a `gitdir:` pointer, and a linked worktree's config
/// lives in the common dir named by its `commondir` file.
fn git_config_remote_url(root: &Utf8Path, remote: &str) -> Option<String> {
	let git_path = root.join(".git");
	let config_path = if git_path.is_file() {
		let pointer = std::fs::read_to_string(&git_path).ok()?;
		let gitdir = pointer.strip_prefix("gitdir:")?.trim();
		let gitdir = if Utf8Path::new(gitdir).is_absolute() {
			Utf8PathBuf::from(gitdir)
		} else {
			root.join(gitdir)
		};
		let common = match std::fs::read_to_string(gitdir.join("commondir")) {
			Ok(rel) => gitdir.join(rel.trim()),
			Err(_) => gitdir,
		};
		common.join("config")
	} else {
		git_path.join("config")
	};
	parse_git_config_remote_url(&std::fs::read_to_string(config_path).ok()?, remote)
}

/// Minimal INI walk for the one key we need. Not a general git
/// config parser: no includes, no quoting/escape handling beyond
/// git's own literal section headers — `git remote add` writes
/// exactly `[remote "name"]` and `\turl = <raw url>`.
fn parse_git_config_remote_url(config: &str, remote: &str) -> Option<String> {
	let header = format!("[remote \"{remote}\"]");
	let mut in_section = false;
	for line in config.lines() {
		let line = line.trim();
		if line.starts_with('[') {
			in_section = line == header;
			continue;
		}
		if !in_section {
			continue;
		}
		if let Some(rest) = line.strip_prefix("url") {
			if let Some(value) = rest.trim_start().strip_prefix('=') {
				let value = value.trim();
				if !value.is_empty() {
					return Some(value.to_string());
				}
			}
		}
	}
	None
}

// ---------------------------------------------------------------
// fj keys.json — the Forgejo host list and per-host tokens.
// ---------------------------------------------------------------

/// Candidate paths for fj's `keys.json`, most-specific first. fj
/// resolves its data dir via the `directories` crate; we mirror the
/// shapes that produces on Linux and macOS (plus fj's legacy macOS
/// location). The container path matches because moon-ide mounts the
/// host dir at `~/.local/share/forgejo-cli` (see containers.md).
fn fj_keys_paths() -> Vec<std::path::PathBuf> {
	let mut candidates = Vec::new();
	if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
		if !xdg.is_empty() {
			candidates.push(std::path::PathBuf::from(xdg).join("forgejo-cli/keys.json"));
		}
	}
	if let Some(home) = std::env::var_os("HOME") {
		let home = std::path::PathBuf::from(home);
		candidates.push(home.join(".local/share/forgejo-cli/keys.json"));
		candidates.push(home.join("Library/Application Support/forgejo-cli.forgejo-cli/keys.json"));
		candidates.push(home.join("Library/Application Support/Cyborus.forgejo-cli/keys.json"));
	}
	candidates
}

fn read_fj_keys() -> Option<String> {
	fj_keys_paths()
		.into_iter()
		.find_map(|path| std::fs::read_to_string(path).ok())
}

/// Hosts the user has authenticated against with fj — our signal
/// that a non-codeberg host is a Forgejo instance at all. Includes
/// alias names and their targets so a remote using either side of an
/// `fj auth` alias matches. Empty set when fj was never used.
fn fj_known_hosts() -> BTreeSet<String> {
	read_fj_keys().map(|json| parse_fj_hosts(&json)).unwrap_or_default()
}

/// Extract the host set from a `keys.json` document. Keys of the
/// `hosts` map are fj's `host_name` form (host plus optional
/// sub-path, no scheme); we index by the bare host segment since
/// that's what a git remote URL carries.
fn parse_fj_hosts(json: &str) -> BTreeSet<String> {
	let mut hosts = BTreeSet::new();
	let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else {
		return hosts;
	};
	let mut push = |name: &str| {
		let bare = name.split('/').next().unwrap_or(name);
		let bare = bare.split(':').next().unwrap_or(bare);
		if !bare.is_empty() {
			hosts.insert(bare.to_owned());
		}
	};
	if let Some(map) = value.get("hosts").and_then(|h| h.as_object()) {
		for key in map.keys() {
			push(key);
		}
	}
	if let Some(map) = value.get("aliases").and_then(|a| a.as_object()) {
		for (key, target) in map {
			push(key);
			if let Some(target) = target.as_str() {
				push(target);
			}
		}
	}
	hosts
}

/// Token fj stores for `host`, if any. Handles both login shapes fj
/// writes: `{"Application":{"token":…}}` and `{"OAuth":{"token":…}}`.
/// An expired OAuth token is still returned — the request path
/// handles the 401 by asking fj to refresh (see
/// [`forgejo_request_with_refresh`]).
fn fj_token_for(host: &str) -> Option<String> {
	let json = read_fj_keys()?;
	parse_fj_token(&json, host)
}

fn parse_fj_token(json: &str, host: &str) -> Option<String> {
	let value = serde_json::from_str::<serde_json::Value>(json).ok()?;
	let bare_host = host.split(':').next().unwrap_or(host);
	let hosts = value.get("hosts")?.as_object()?;
	let login = hosts.iter().find_map(|(key, login)| {
		let key_host = key.split('/').next().unwrap_or(key);
		let key_host = key_host.split(':').next().unwrap_or(key_host);
		(key_host == bare_host).then_some(login)
	})?;
	let token = login
		.get("Application")
		.or_else(|| login.get("OAuth"))?
		.get("token")?
		.as_str()?;
	if token.is_empty() {
		return None;
	}
	Some(token.to_owned())
}

/// Ask fj to touch the API for `host` so it refreshes (and saves) an
/// expired OAuth token. `fj whoami --host <host>` is the cheapest
/// authenticated call fj offers. Best-effort: a missing fj binary or
/// a failed refresh just means the retry will fail with the original
/// 401, which is the right error to surface.
async fn fj_refresh_login(host: &str) -> bool {
	let mut cmd = tokio::process::Command::new("fj");
	cmd
		.args(["whoami", "--host", host])
		.env("LC_ALL", "C")
		.stdin(std::process::Stdio::null())
		.stdout(std::process::Stdio::null())
		.stderr(std::process::Stdio::null());
	let Ok(child) = cmd.spawn() else {
		return false;
	};
	matches!(
		tokio::time::timeout(FORGEJO_TIMEOUT, child.wait_with_output()).await,
		Ok(Ok(output)) if output.status.success()
	)
}

// ---------------------------------------------------------------
// Forgejo REST client.
// ---------------------------------------------------------------

/// Same ceiling as the gh call sites — interactive UI features would
/// rather fail than freeze.
const FORGEJO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// One open PR as the branch switcher / SCM panel / review publisher
/// consume it. Field names follow gh's JSON vocabulary so the shared
/// row-building code reads the same for both forges.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForgejoPr {
	pub number: u64,
	pub title: String,
	pub author: String,
	pub head_ref: String,
	pub head_sha: String,
	pub is_draft: bool,
	/// RFC 3339 as the API reports it; relative formatting happens
	/// at the call site with the same helper the gh path uses.
	pub updated_at: String,
	pub html_url: String,
	pub assignees: Vec<String>,
	pub requested_reviewers: Vec<String>,
}

fn forgejo_client() -> Result<reqwest::Client, String> {
	reqwest::Client::builder()
		.timeout(FORGEJO_TIMEOUT)
		.user_agent("moon-ide")
		.build()
		.map_err(|e| format!("http client: {e}"))
}

/// Issue one Forgejo API request, refreshing the fj login once on a
/// 401 (expired OAuth token) before retrying. `body` (with method
/// POST) turns the request into a JSON POST; `None` is a GET.
async fn forgejo_request_with_refresh(
	remote: &ForgeRemote,
	path_and_query: &str,
	body: Option<&serde_json::Value>,
) -> Result<serde_json::Value, String> {
	let mut refreshed = false;
	loop {
		let token = fj_token_for(&remote.host);
		if body.is_some() && token.is_none() {
			// Reads work anonymously on public repos; writes never do.
			return Err(format!(
				"no fj login for {} — run `fj auth login` on the host",
				remote.host
			));
		}
		let client = forgejo_client()?;
		let url = format!("{}{}", remote.api_base(), path_and_query);
		let mut request = match body {
			Some(json) => client.post(&url).json(json),
			None => client.get(&url),
		};
		if let Some(token) = &token {
			request = request.header("Authorization", format!("token {token}"));
		}
		let response = request.send().await.map_err(|e| format!("{}: {e}", remote.host))?;
		let status = response.status();
		if status == reqwest::StatusCode::UNAUTHORIZED && !refreshed && token.is_some() {
			refreshed = true;
			if fj_refresh_login(&remote.host).await {
				continue;
			}
		}
		if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
			return Err(format!(
				"{} returned {} — run `fj auth login` on the host to (re)authenticate",
				remote.host,
				status.as_u16()
			));
		}
		if !status.is_success() {
			let detail = response.text().await.unwrap_or_default();
			let detail = extract_forgejo_message(&detail).unwrap_or(detail);
			return Err(format!(
				"{} returned {}: {}",
				remote.host,
				status.as_u16(),
				detail.trim()
			));
		}
		return response
			.json()
			.await
			.map_err(|e| format!("{}: bad JSON: {e}", remote.host));
	}
}

/// Pull `{"message": …}` out of a Forgejo error body so the user
/// sees the actionable part, not a JSON blob.
fn extract_forgejo_message(body: &str) -> Option<String> {
	let value = serde_json::from_str::<serde_json::Value>(body).ok()?;
	value.get("message")?.as_str().map(str::to_owned)
}

/// Open PRs, most recently updated first, capped server-side at
/// `limit`. Anonymous on public repos; authenticated when fj has a
/// login for the host.
pub async fn forgejo_list_open_prs(remote: &ForgeRemote, limit: usize) -> Result<Vec<ForgejoPr>, String> {
	let path = format!(
		"/repos/{}/pulls?state=open&sort=recentupdate&limit={limit}",
		remote.owner_repo
	);
	let value = forgejo_request_with_refresh(remote, &path, None).await?;
	Ok(parse_forgejo_pr_list(&value))
}

/// Login of the token's user — needed to filter "participating"
/// PRs, since Forgejo's list API has no `involves:@me` equivalent.
pub async fn forgejo_current_user(remote: &ForgeRemote) -> Result<String, String> {
	let value = forgejo_request_with_refresh(remote, "/user", None).await?;
	value
		.get("login")
		.and_then(|l| l.as_str())
		.filter(|l| !l.is_empty())
		.map(str::to_owned)
		.ok_or_else(|| format!("{}: /user returned no login", remote.host))
}

/// POST a review to `pulls/{number}/reviews`. Returns the review's
/// `html_url` (empty when the API omits it — old instances).
pub async fn forgejo_post_review(
	remote: &ForgeRemote,
	number: u64,
	payload: &serde_json::Value,
) -> Result<String, String> {
	let path = format!("/repos/{}/pulls/{number}/reviews", remote.owner_repo);
	let value = forgejo_request_with_refresh(remote, &path, Some(payload)).await?;
	Ok(
		value
			.get("html_url")
			.and_then(|u| u.as_str())
			.unwrap_or_default()
			.to_owned(),
	)
}

/// Parse the `/pulls` array into [`ForgejoPr`] rows. Skips rows
/// missing required fields rather than erroring — a schema drift
/// shouldn't take the whole palette down.
fn parse_forgejo_pr_list(value: &serde_json::Value) -> Vec<ForgejoPr> {
	let Some(arr) = value.as_array() else {
		return Vec::new();
	};
	let logins = |field: &serde_json::Value| -> Vec<String> {
		field
			.as_array()
			.map(|users| {
				users
					.iter()
					.filter_map(|u| u.get("login").and_then(|l| l.as_str()))
					.map(str::to_owned)
					.collect()
			})
			.unwrap_or_default()
	};
	let mut rows = Vec::with_capacity(arr.len());
	for item in arr {
		let Some(number) = item.get("number").and_then(|n| n.as_u64()) else {
			continue;
		};
		let Some(title) = item.get("title").and_then(|t| t.as_str()) else {
			continue;
		};
		let Some(head) = item.get("head") else {
			continue;
		};
		let Some(head_ref) = head.get("ref").and_then(|r| r.as_str()) else {
			continue;
		};
		rows.push(ForgejoPr {
			number,
			title: title.to_owned(),
			author: item
				.get("user")
				.and_then(|u| u.get("login"))
				.and_then(|l| l.as_str())
				.unwrap_or("")
				.to_owned(),
			head_ref: head_ref.to_owned(),
			head_sha: head.get("sha").and_then(|s| s.as_str()).unwrap_or("").to_owned(),
			is_draft: item.get("draft").and_then(|d| d.as_bool()).unwrap_or(false),
			updated_at: item.get("updated_at").and_then(|u| u.as_str()).unwrap_or("").to_owned(),
			html_url: item.get("html_url").and_then(|u| u.as_str()).unwrap_or("").to_owned(),
			assignees: item.get("assignees").map(&logins).unwrap_or_default(),
			requested_reviewers: item.get("requested_reviewers").map(&logins).unwrap_or_default(),
		});
	}
	rows
}

#[cfg(test)]
mod tests {
	use super::*;

	fn no_forgejo() -> BTreeSet<String> {
		BTreeSet::new()
	}

	#[test]
	fn normalize_remote_url_handles_all_shapes() {
		let web = |raw: &str| normalize_remote_url_with(raw, &no_forgejo()).map(|r| r.web_base);
		// SCP-style SSH is what `git clone git@github.com:...` leaves
		// behind.
		assert_eq!(
			web("git@github.com:moon/ide.git"),
			Some("https://github.com/moon/ide".into()),
		);
		assert_eq!(
			web("git@github.com:moon/ide"),
			Some("https://github.com/moon/ide".into()),
		);
		// Explicit SSH URL with and without the `git@` user.
		assert_eq!(
			web("ssh://git@github.com/moon/ide.git"),
			Some("https://github.com/moon/ide".into()),
		);
		assert_eq!(
			web("ssh://github.com/moon/ide.git"),
			Some("https://github.com/moon/ide".into()),
		);
		// HTTPS is already close to right, we just trim `.git`.
		assert_eq!(
			web("https://github.com/moon/ide.git"),
			Some("https://github.com/moon/ide".into()),
		);
		assert_eq!(
			web("https://github.com/moon/ide"),
			Some("https://github.com/moon/ide".into()),
		);
		// Unknown hosts are rejected until we add mapping for them —
		// better to leave the frontend un-linkified than to guess at
		// a URL convention.
		assert_eq!(web("https://gitlab.com/moon/ide.git"), None);
		assert_eq!(web("git@bitbucket.org:moon/ide.git"), None);
		assert_eq!(web(""), None);
	}

	#[test]
	fn normalize_remote_url_recognises_forgejo_hosts() {
		// codeberg.org needs no fj login to be recognised.
		let remote = normalize_remote_url_with("git@codeberg.org:moon/ide.git", &no_forgejo()).unwrap();
		assert_eq!(remote.kind, ForgeKind::Forgejo);
		assert_eq!(remote.web_base, "https://codeberg.org/moon/ide");
		assert_eq!(remote.owner_repo, "moon/ide");
		assert_eq!(remote.host, "codeberg.org");

		// Self-hosted instances match only when fj knows the host.
		assert_eq!(
			normalize_remote_url_with("git@git.example.com:moon/ide.git", &no_forgejo()),
			None
		);
		let known: BTreeSet<String> = ["git.example.com".to_owned()].into();
		let remote = normalize_remote_url_with("git@git.example.com:moon/ide.git", &known).unwrap();
		assert_eq!(remote.kind, ForgeKind::Forgejo);
		assert_eq!(remote.web_base, "https://git.example.com/moon/ide");

		// SSH ports are not web ports — dropped from the identity.
		let remote = normalize_remote_url_with("ssh://git@git.example.com:2222/moon/ide.git", &known).unwrap();
		assert_eq!(remote.web_base, "https://git.example.com/moon/ide");

		// GitHub stays GitHub even with fj hosts configured.
		let remote = normalize_remote_url_with("git@github.com:moon/ide.git", &known).unwrap();
		assert_eq!(remote.kind, ForgeKind::GitHub);
	}

	#[test]
	fn parse_git_config_remote_url_walks_sections() {
		let config = "[core]\n\tbare = false\n[remote \"origin\"]\n\turl = git@github.com:moon/ide.git\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n[remote \"upstream\"]\n\turl = https://github.com/up/stream.git\n";
		assert_eq!(
			parse_git_config_remote_url(config, "origin"),
			Some("git@github.com:moon/ide.git".into())
		);
		assert_eq!(
			parse_git_config_remote_url(config, "upstream"),
			Some("https://github.com/up/stream.git".into())
		);
		assert_eq!(parse_git_config_remote_url(config, "fork"), None);
		// A `url` key outside any remote section must not match.
		assert_eq!(parse_git_config_remote_url("[core]\n\turl = x\n", "origin"), None);
	}

	#[test]
	fn parse_fj_hosts_collects_hosts_and_aliases() {
		let json = r#"{
			"hosts": {
				"codeberg.org": { "Application": { "token": "tok1" } },
				"git.example.com/forgejo": { "OAuth": { "token": "tok2", "refresh_token": "r", "expires_at": [2026, 1] } }
			},
			"aliases": { "cb": "codeberg.org", "work": "forge.corp.net" },
			"default_ssh": []
		}"#;
		let hosts = parse_fj_hosts(json);
		assert!(hosts.contains("codeberg.org"));
		assert!(hosts.contains("git.example.com"));
		assert!(hosts.contains("cb"));
		assert!(hosts.contains("forge.corp.net"));
		assert!(parse_fj_hosts("not json").is_empty());
		assert!(parse_fj_hosts("{}").is_empty());
	}

	#[test]
	fn parse_fj_token_handles_both_login_shapes() {
		let json = r#"{
			"hosts": {
				"codeberg.org": { "Application": { "token": "app-tok" } },
				"git.example.com": { "OAuth": { "token": "oauth-tok", "refresh_token": "r", "expires_at": [2026, 1] } }
			}
		}"#;
		assert_eq!(parse_fj_token(json, "codeberg.org"), Some("app-tok".into()));
		assert_eq!(parse_fj_token(json, "git.example.com"), Some("oauth-tok".into()));
		assert_eq!(parse_fj_token(json, "unknown.org"), None);
		assert_eq!(parse_fj_token("not json", "codeberg.org"), None);
	}

	#[test]
	fn parse_forgejo_pr_list_maps_fields_and_skips_broken_rows() {
		let json: serde_json::Value = serde_json::from_str(
			r#"[
			{
				"number": 42,
				"title": "Add feature",
				"user": { "login": "alice" },
				"head": { "ref": "feat/x", "sha": "abc123" },
				"draft": true,
				"updated_at": "2026-01-02T03:04:05Z",
				"html_url": "https://codeberg.org/moon/ide/pulls/42",
				"assignees": [ { "login": "bob" } ],
				"requested_reviewers": [ { "login": "carol" } ]
			},
			{ "number": 43, "title": "No head — skipped" }
		]"#,
		)
		.unwrap();
		let rows = parse_forgejo_pr_list(&json);
		assert_eq!(rows.len(), 1);
		let pr = &rows[0];
		assert_eq!(pr.number, 42);
		assert_eq!(pr.title, "Add feature");
		assert_eq!(pr.author, "alice");
		assert_eq!(pr.head_ref, "feat/x");
		assert_eq!(pr.head_sha, "abc123");
		assert!(pr.is_draft);
		assert_eq!(pr.updated_at, "2026-01-02T03:04:05Z");
		assert_eq!(pr.html_url, "https://codeberg.org/moon/ide/pulls/42");
		assert_eq!(pr.assignees, vec!["bob".to_owned()]);
		assert_eq!(pr.requested_reviewers, vec!["carol".to_owned()]);
	}
}
