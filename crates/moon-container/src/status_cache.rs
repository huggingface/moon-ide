//! Process-wide TTL cache in front of `docker compose ps`.
//!
//! Why this exists
//! ---------------
//!
//! Every LSP IPC routes through `ensure_broker` (Tauri layer),
//! which probes `Workspace::status()` to pick host-vs-container
//! routing. Each probe shells out to `docker compose -f … ps
//! --all --format json`. A single keystroke inside a TS template
//! string can fan into didChange, completion, pull-diagnostics
//! and hover, sometimes per-language; with several open buffers
//! and bound folders, that's 30+ `docker compose ps` calls in
//! under a second. Each shell-out is 50–200 ms and blocks both
//! the daemon and the IDE — the editor visibly freezes.
//!
//! Shape
//! -----
//!
//! - Key: the *compose project name* + the absolute path to the
//!   compose file. Together they uniquely identify a `docker
//!   compose` lifecycle: project name pins `-p`, compose path
//!   pins `-f`, and a `Workspace` / `ProjectCompose` always
//!   passes both flags explicitly.
//! - Value: the last `Ok(ContainerStatus)` and when it was
//!   observed. Errors are **not** cached — a transient daemon
//!   hiccup shouldn't pin the routing decision for a full TTL.
//! - TTL: [`STATUS_TTL`] (currently 1 s). Long enough to collapse
//!   the per-keystroke fanout into a single probe; short enough
//!   that an external `docker compose down` reflects within the
//!   same window the folder-bar already polls at (2 s).
//! - Mutating callers ([`Workspace::setup`], `pause`, `resume`,
//!   `rebuild`, `stop`, `teardown`, and the [`ProjectCompose`]
//!   equivalents) call [`invalidate`] after the mutation
//!   completes. That way an internal `pause()` followed by
//!   `status()` returns the fresh `Paused` state immediately,
//!   not the pre-pause cached `Running`.
//!
//! Scope
//! -----
//!
//! Strictly an optimisation for the read path. Does **not**
//! serialise concurrent mutations or coalesce in-flight `ps`
//! calls — two simultaneous cache misses each shell out, both
//! write back, last writer wins. That's fine: `ps` is idempotent
//! and the duplicate is rare (the first cache fill ends the
//! race for the next [`STATUS_TTL`]).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use camino::{Utf8Path, Utf8PathBuf};
use moon_protocol::container::ContainerStatus;

use crate::project::ProjectName;

/// How long a successful `status()` reading is reused before the
/// next caller re-probes `docker compose ps`.
pub const STATUS_TTL: Duration = Duration::from_millis(1000);

/// Identifier for a single compose lifecycle. Equal keys see
/// identical `docker compose -f <path> -p <name> ps` output;
/// distinct keys must not share cache slots.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CacheKey {
	project: String,
	compose_path: Utf8PathBuf,
}

impl CacheKey {
	fn new(project: &ProjectName, compose_path: &Utf8Path) -> Self {
		Self {
			project: project.as_str().to_owned(),
			compose_path: compose_path.to_owned(),
		}
	}
}

struct Entry {
	at: Instant,
	status: ContainerStatus,
}

static CACHE: Mutex<Option<HashMap<CacheKey, Entry>>> = Mutex::new(None);

fn with_cache<R>(f: impl FnOnce(&mut HashMap<CacheKey, Entry>) -> R) -> R {
	let mut guard = CACHE.lock().expect("status cache mutex poisoned");
	let map = guard.get_or_insert_with(HashMap::new);
	f(map)
}

/// Read-through helper: returns the cached `ContainerStatus`
/// when the entry is younger than [`STATUS_TTL`], otherwise calls
/// `fetch`, stores any `Ok(…)` result, and returns it. Errors
/// pass through uncached so the next caller retries immediately.
pub(crate) async fn get_or_fetch<F, Fut, E>(
	project: &ProjectName,
	compose_path: &Utf8Path,
	now: Instant,
	fetch: F,
) -> Result<ContainerStatus, E>
where
	F: FnOnce() -> Fut,
	Fut: std::future::Future<Output = Result<ContainerStatus, E>>,
{
	let key = CacheKey::new(project, compose_path);
	if let Some(hit) = with_cache(|map| {
		map.get(&key).and_then(|entry| {
			if now.duration_since(entry.at) < STATUS_TTL {
				Some(entry.status.clone())
			} else {
				None
			}
		})
	}) {
		return Ok(hit);
	}

	let status = fetch().await?;
	with_cache(|map| {
		map.insert(
			key,
			Entry {
				at: now,
				status: status.clone(),
			},
		);
	});
	Ok(status)
}

/// Drop the cached reading for a compose lifecycle so the next
/// `status()` re-probes. Mutating commands call this after they
/// succeed (and after a failure that may have left the daemon
/// in an indeterminate state) — every observable state change
/// the IDE initiates flushes the cache.
pub(crate) fn invalidate(project: &ProjectName, compose_path: &Utf8Path) {
	let key = CacheKey::new(project, compose_path);
	with_cache(|map| {
		map.remove(&key);
	});
}

#[cfg(test)]
mod tests {
	use std::sync::atomic::{AtomicUsize, Ordering};
	use std::sync::Arc;

	use moon_protocol::container::ContainerState;

	use super::*;

	/// Distinct cache key per test. The cache is process-wide, so
	/// tests that ran concurrently against the same key would
	/// otherwise see each other's writes — the `cargo test`
	/// default is parallel, and reaching for `--test-threads=1`
	/// is worse than just keying tests by their own name.
	fn make_key(test_id: &str) -> (ProjectName, Utf8PathBuf) {
		let project = crate::project::project_name_for_id(test_id).unwrap();
		let compose = Utf8PathBuf::from(format!("/tmp/moon-status-cache-test-{test_id}/compose.yaml"));
		(project, compose)
	}

	fn running() -> ContainerStatus {
		ContainerStatus {
			state: ContainerState::Running,
			services: Vec::new(),
		}
	}

	fn paused() -> ContainerStatus {
		ContainerStatus {
			state: ContainerState::Paused,
			services: Vec::new(),
		}
	}

	#[tokio::test]
	async fn cache_hit_within_ttl_skips_fetcher() {
		let (project, compose) = make_key("cache-hit-within-ttl");
		let calls = Arc::new(AtomicUsize::new(0));

		let now = Instant::now();
		let c = calls.clone();
		let first = get_or_fetch::<_, _, ()>(&project, &compose, now, || async move {
			c.fetch_add(1, Ordering::SeqCst);
			Ok(running())
		})
		.await
		.unwrap();
		assert_eq!(first.state, ContainerState::Running);
		assert_eq!(calls.load(Ordering::SeqCst), 1);

		// Same `now` instant — emphatically inside the TTL window.
		let c = calls.clone();
		let second = get_or_fetch::<_, _, ()>(&project, &compose, now, || async move {
			c.fetch_add(1, Ordering::SeqCst);
			Ok(paused())
		})
		.await
		.unwrap();
		// Cached `Running` returned even though the fetcher would
		// have produced `Paused`.
		assert_eq!(second.state, ContainerState::Running);
		assert_eq!(calls.load(Ordering::SeqCst), 1);
	}

	#[tokio::test]
	async fn cache_miss_after_ttl_refetches() {
		let (project, compose) = make_key("cache-miss-after-ttl");
		let calls = Arc::new(AtomicUsize::new(0));

		let t0 = Instant::now();
		let c = calls.clone();
		get_or_fetch::<_, _, ()>(&project, &compose, t0, || async move {
			c.fetch_add(1, Ordering::SeqCst);
			Ok(running())
		})
		.await
		.unwrap();

		// Step past the TTL — fetcher must run again.
		let t1 = t0 + STATUS_TTL + Duration::from_millis(1);
		let c = calls.clone();
		let again = get_or_fetch::<_, _, ()>(&project, &compose, t1, || async move {
			c.fetch_add(1, Ordering::SeqCst);
			Ok(paused())
		})
		.await
		.unwrap();
		assert_eq!(again.state, ContainerState::Paused);
		assert_eq!(calls.load(Ordering::SeqCst), 2);
	}

	#[tokio::test]
	async fn invalidate_forces_refetch_even_inside_ttl() {
		let (project, compose) = make_key("invalidate-forces-refetch");
		let calls = Arc::new(AtomicUsize::new(0));

		let now = Instant::now();
		let c = calls.clone();
		get_or_fetch::<_, _, ()>(&project, &compose, now, || async move {
			c.fetch_add(1, Ordering::SeqCst);
			Ok(running())
		})
		.await
		.unwrap();

		invalidate(&project, &compose);

		let c = calls.clone();
		let after = get_or_fetch::<_, _, ()>(&project, &compose, now, || async move {
			c.fetch_add(1, Ordering::SeqCst);
			Ok(paused())
		})
		.await
		.unwrap();
		assert_eq!(after.state, ContainerState::Paused);
		assert_eq!(calls.load(Ordering::SeqCst), 2);
	}

	#[tokio::test]
	async fn errors_are_not_cached() {
		let (project, compose) = make_key("errors-are-not-cached");
		let calls = Arc::new(AtomicUsize::new(0));
		let now = Instant::now();

		// First call: fetcher errors. The entry must not be
		// stored, otherwise a daemon hiccup would pin the
		// routing decision for a whole TTL.
		let c = calls.clone();
		let err: Result<ContainerStatus, &'static str> = get_or_fetch(&project, &compose, now, || async move {
			c.fetch_add(1, Ordering::SeqCst);
			Err("boom")
		})
		.await;
		assert!(err.is_err());

		// Second call (same instant — still inside TTL) must
		// re-invoke the fetcher, this time succeeding.
		let c = calls.clone();
		let ok = get_or_fetch::<_, _, &'static str>(&project, &compose, now, || async move {
			c.fetch_add(1, Ordering::SeqCst);
			Ok(running())
		})
		.await
		.unwrap();
		assert_eq!(ok.state, ContainerState::Running);
		assert_eq!(calls.load(Ordering::SeqCst), 2);
	}

	#[tokio::test]
	async fn distinct_keys_do_not_share_slots() {
		let project_a = crate::project::project_name_for_id("distinct-keys-alpha").unwrap();
		let project_b = crate::project::project_name_for_id("distinct-keys-beta").unwrap();
		let compose = Utf8PathBuf::from("/tmp/moon-status-cache-test-distinct/compose.yaml");
		let now = Instant::now();

		get_or_fetch::<_, _, ()>(&project_a, &compose, now, || async { Ok(running()) })
			.await
			.unwrap();

		let calls = Arc::new(AtomicUsize::new(0));
		let c = calls.clone();
		get_or_fetch::<_, _, ()>(&project_b, &compose, now, || async move {
			c.fetch_add(1, Ordering::SeqCst);
			Ok(paused())
		})
		.await
		.unwrap();
		// `project_b` had no prior entry — its fetcher must run
		// even though `project_a` cached at the same instant.
		assert_eq!(calls.load(Ordering::SeqCst), 1);
	}
}
