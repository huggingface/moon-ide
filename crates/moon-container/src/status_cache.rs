//! Process-wide TTL cache + single-flight in front of
//! `docker compose ps`.
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
//! under a second, mostly issued **concurrently** (the frontend
//! fires them in parallel through Tauri's command runtime).
//! Each shell-out is 50–200 ms and blocks both the daemon and
//! the IDE — the editor visibly freezes.
//!
//! Shape
//! -----
//!
//! - Key: the *compose project name* + the absolute path to the
//!   compose file. Together they uniquely identify a `docker
//!   compose` lifecycle: project name pins `-p`, compose path
//!   pins `-f`, and a `Workspace` / `ProjectCompose` always
//!   passes both flags explicitly.
//! - Per key: an `Arc<Slot>` carrying a `tokio::sync::Mutex`
//!   guarding the slot's last `Ok(ContainerStatus)` plus the
//!   instant it was observed. The async mutex is what gives us
//!   single-flight: concurrent miss callers serialise behind
//!   the leader and pick up the leader's cached result on
//!   acquire instead of each shelling out themselves.
//! - TTL: [`STATUS_TTL`] (currently 1 s). Long enough to
//!   collapse a per-keystroke burst into one shell-out; short
//!   enough that an external `docker compose down` reflects
//!   within the same window the folder-bar already polls at
//!   (2 s).
//! - Errors are **not** stored. A transient daemon hiccup
//!   shouldn't pin the routing decision for a full TTL. A
//!   failed leader fetch returns the error to its caller and
//!   leaves the slot untouched; the next caller acquires the
//!   slot mutex, sees no fresh entry, and tries again. Under
//!   sustained failure that's one sequential retry per IPC,
//!   not 20 concurrent retries — strictly an improvement, and
//!   the daemon-healthy steady state is what we optimise for.
//! - Mutating callers ([`Workspace::setup`], `pause`, `resume`,
//!   `rebuild`, `stop`, `teardown`, and the [`ProjectCompose`]
//!   equivalents) call [`invalidate`] after the mutation
//!   completes. The slot's `Entry` is reset to `None`; the
//!   slot itself stays so an in-flight leader's fetch still
//!   serves any followers already queued.
//!
//! Scope
//! -----
//!
//! Single-flight is per key only. Two distinct projects whose
//! compose files happen to share a daemon are still allowed to
//! shell out concurrently — they're independent lifecycles and
//! a folder-bar full of projects wants its own probe per row.
//! Cross-project coalescing isn't worth the complexity for the
//! call volumes we see (a workspace has on the order of a
//! handful of projects, not thousands).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use camino::{Utf8Path, Utf8PathBuf};
use moon_protocol::container::ContainerStatus;
use tokio::sync::Mutex as AsyncMutex;

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

#[derive(Clone)]
struct Entry {
	at: Instant,
	status: ContainerStatus,
}

/// Per-key serialisation point. Holding [`Slot::entry`] across a
/// fetch is what coalesces concurrent miss callers into a single
/// shell-out.
struct Slot {
	entry: AsyncMutex<Option<Entry>>,
}

impl Slot {
	fn new() -> Self {
		Self {
			entry: AsyncMutex::new(None),
		}
	}
}

static CACHE: Mutex<Option<HashMap<CacheKey, Arc<Slot>>>> = Mutex::new(None);

/// Look up or insert the per-key slot. Synchronous — the
/// `std::sync::Mutex` is only ever held long enough to clone an
/// `Arc`, so no async work runs while we hold it.
fn slot_for(key: &CacheKey) -> Arc<Slot> {
	let mut guard = CACHE.lock().expect("status cache mutex poisoned");
	let map = guard.get_or_insert_with(HashMap::new);
	map.entry(key.clone()).or_insert_with(|| Arc::new(Slot::new())).clone()
}

/// Read-through helper with single-flight semantics: at most one
/// concurrent `fetch()` per `(project, compose_path)` key. The
/// leader caches its `Ok(…)` result under the slot mutex; any
/// followers that were queued behind it pick up that cached
/// result on acquire and return without re-fetching. Errors
/// propagate to the leader's caller without being stored, so the
/// next caller (after the leader releases) tries again.
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
	let slot = slot_for(&key);

	// Fast path: a recent fresh entry is visible without
	// blocking on anyone else. `try_lock` keeps the steady-state
	// hit cheap even under contention — if a leader is mid-fetch
	// we fall through to `lock().await` below and join the
	// queue.
	if let Ok(guard) = slot.entry.try_lock() {
		if let Some(entry) = guard.as_ref() {
			if now.duration_since(entry.at) < STATUS_TTL {
				return Ok(entry.status.clone());
			}
		}
	}

	// Slow path: serialise behind the slot mutex. Either we're
	// the first miss in this burst (we'll do the fetch), or
	// someone else is already fetching (we'll await them and
	// inherit their result).
	let mut guard = slot.entry.lock().await;

	// Re-check freshness now that we hold the lock: if the
	// leader populated while we waited, take the cached entry
	// and return without fetching.
	if let Some(entry) = guard.as_ref() {
		if now.duration_since(entry.at) < STATUS_TTL {
			return Ok(entry.status.clone());
		}
	}

	let status = fetch().await?;
	*guard = Some(Entry {
		at: now,
		status: status.clone(),
	});
	Ok(status)
}

/// Drop the cached reading for a compose lifecycle so the next
/// `status()` re-probes. Mutating commands call this after they
/// succeed — every observable state change the IDE initiates
/// flushes the cache. The slot itself is preserved so any
/// follower that's currently queued behind a leader still
/// benefits from single-flight; only the cached `Entry` is
/// cleared.
///
/// Async because the per-slot mutex is a `tokio::sync::Mutex`
/// (we hold it across the `ps` shell-out). In the worst case (a
/// leader is mid-fetch when the invalidator arrives) the call
/// waits ~150 ms for the leader to finish, then clears its
/// freshly-fetched value — exactly what we want, since that
/// value would be the pre-mutation reading.
pub(crate) async fn invalidate(project: &ProjectName, compose_path: &Utf8Path) {
	let key = CacheKey::new(project, compose_path);
	let slot = slot_for(&key);
	let mut entry = slot.entry.lock().await;
	*entry = None;
}

#[cfg(test)]
mod tests {
	use std::sync::atomic::{AtomicUsize, Ordering};
	use std::sync::Arc;

	use moon_protocol::container::ContainerState;
	use tokio::sync::Barrier;

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

		invalidate(&project, &compose).await;

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

	/// The actual bug this module exists to fix: when N callers
	/// race into `get_or_fetch` with an empty slot, exactly one
	/// of them shells out — the rest serialise behind the
	/// per-slot async mutex, see the leader's fresh entry on
	/// acquire, and return it without re-fetching.
	///
	/// The fetcher uses a barrier so we don't depend on timing:
	/// we explicitly hold the leader open until every caller has
	/// reached `get_or_fetch`. That guarantees every miss path
	/// has actually started (and queued behind the leader)
	/// before the leader is allowed to complete.
	/// The actual bug this module exists to fix: when N callers
	/// race into `get_or_fetch` with an empty slot, exactly one
	/// of them shells out — the rest serialise behind the
	/// per-slot async mutex, see the leader's fresh entry on
	/// acquire, and return it without re-fetching.
	///
	/// We use a 2-party barrier between the leader's fetcher and
	/// the test driver so we don't depend on timing: the leader
	/// suspends inside its fetcher until the test side has had
	/// enough scheduler turns to queue every follower behind the
	/// slot mutex. Only then does the test trip the barrier, the
	/// leader completes, and the followers wake to find the
	/// cached entry.
	#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
	async fn concurrent_callers_coalesce_to_one_fetch() {
		let (project, compose) = make_key("concurrent-coalesce");
		let calls = Arc::new(AtomicUsize::new(0));
		let now = Instant::now();
		const CONCURRENCY: usize = 20;
		// Only the leader invokes the fetcher (it's `FnOnce`),
		// so only one waiter sits on the barrier from inside —
		// the other party is the test driver itself.
		let barrier = Arc::new(Barrier::new(2));

		let mut tasks = Vec::with_capacity(CONCURRENCY);
		for _ in 0..CONCURRENCY {
			let project = project.clone();
			let compose = compose.clone();
			let calls = calls.clone();
			let barrier = barrier.clone();
			tasks.push(tokio::spawn(async move {
				get_or_fetch::<_, _, ()>(&project, &compose, now, || async move {
					calls.fetch_add(1, Ordering::SeqCst);
					barrier.wait().await;
					Ok(running())
				})
				.await
				.unwrap()
			}));
		}

		// Give every spawned task enough turns to reach the
		// async-mutex acquire and queue behind the leader.
		// `yield_now` is one scheduler turn; loop until we
		// observe the calls counter tick (the leader has
		// started its fetcher, so every other caller has
		// already taken its slot lock path).
		while calls.load(Ordering::SeqCst) == 0 {
			tokio::task::yield_now().await;
		}
		// Extra turns: even after the leader is in-flight, the
		// followers might not have reached `lock().await` yet.
		// A handful of yields covers the scheduler's batching.
		for _ in 0..8 {
			tokio::task::yield_now().await;
		}

		barrier.wait().await;

		for t in tasks {
			let s = t.await.unwrap();
			assert_eq!(s.state, ContainerState::Running);
		}
		// The whole point: exactly one shell-out, not 20.
		assert_eq!(calls.load(Ordering::SeqCst), 1);
	}
}
