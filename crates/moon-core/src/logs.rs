//! Diagnostic log sink — the moon-ide-internal counterpart to `tracing`.
//!
//! Why a separate channel
//! ----------------------
//!
//! `tracing` already exists for developer-facing structured logging,
//! but its output lands on whatever stderr the moon-ide process
//! inherited (typically the terminal that launched it, invisible to
//! the user once a packaged build is running). When a user-facing
//! feature looks broken — "Ctrl+S did nothing", "the LSP pill went
//! quiet" — we want **the user** to be able to look at moon-ide's
//! own breadcrumbs without leaving the IDE.
//!
//! [`LogSink`] is that surface: a free-form, source-keyed ring
//! buffer plus a `tokio::broadcast` fan-out. Producers emit
//! through one of the explicit `emit_*` helpers; the Tauri layer
//! subscribes to the broadcast and re-emits each entry on the
//! `logs:entry` event so the frontend's bottom-panel logs view can
//! paint it. New subscribers (e.g. when the user opens the panel
//! for the first time after some entries already exist) read the
//! ring via [`LogSink::snapshot`] and then attach to the live
//! stream.
//!
//! Source naming convention: `<area>.<sub-area>`. The picker uses
//! the prefix purely for visual grouping; nothing else parses it.
//! See [`moon_protocol::logs`] for the wire types.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

pub use moon_protocol::logs::{LogEntry, LogLevel};
use tokio::sync::broadcast;

/// Per-source ring buffer cap. Two thousand lines is enough to
/// cover an entire workspace session of LSP chatter on a busy
/// monorepo without bloating memory; older entries silently
/// trim away the front.
const MAX_PER_SOURCE: usize = 2000;

/// Broadcast channel depth. Tuned for one slow UI consumer plus
/// headroom for short bursts (e.g. a server stderr drain shoving
/// in a couple hundred lines after a crash). Lagged receivers
/// drop the oldest unread entries — that's preferable to back-
/// pressuring producers, which would risk pinning whatever loop
/// is generating logs.
const BROADCAST_CAP: usize = 1024;

pub struct LogSink {
	inner: Mutex<HashMap<String, VecDeque<LogEntry>>>,
	events: broadcast::Sender<LogEntry>,
	next_seq: AtomicU64,
}

impl LogSink {
	pub fn new() -> Arc<Self> {
		let (events, _) = broadcast::channel(BROADCAST_CAP);
		Arc::new(Self {
			inner: Mutex::new(HashMap::new()),
			events,
			next_seq: AtomicU64::new(1),
		})
	}

	/// Push one entry into `source`'s ring and fan it out. Cheap;
	/// callers in hot paths (per-keystroke `editor.completion`
	/// traces, server stderr drains) can call this freely.
	///
	/// The broadcast `send` is best-effort: with no subscribers
	/// the call returns an error which we ignore by design — the
	/// ring still has the entry for the next subscriber to pick up
	/// via `snapshot`. Same posture for the LSP broker's own
	/// event channel.
	pub fn emit(&self, source: &str, level: LogLevel, message: impl Into<String>) {
		let entry = LogEntry {
			source: source.to_owned(),
			level,
			message: message.into(),
			ts_ms: now_ms(),
			seq: self.next_seq.fetch_add(1, Ordering::SeqCst),
		};
		{
			let mut buf = self.inner.lock().expect("log sink mutex poisoned");
			let v = buf.entry(source.to_owned()).or_default();
			v.push_back(entry.clone());
			while v.len() > MAX_PER_SOURCE {
				v.pop_front();
			}
		}
		let _ = self.events.send(entry);
	}

	pub fn debug(&self, source: &str, message: impl Into<String>) {
		self.emit(source, LogLevel::Debug, message);
	}

	pub fn info(&self, source: &str, message: impl Into<String>) {
		self.emit(source, LogLevel::Info, message);
	}

	pub fn warn(&self, source: &str, message: impl Into<String>) {
		self.emit(source, LogLevel::Warn, message);
	}

	pub fn error(&self, source: &str, message: impl Into<String>) {
		self.emit(source, LogLevel::Error, message);
	}

	pub fn subscribe(&self) -> broadcast::Receiver<LogEntry> {
		self.events.subscribe()
	}

	/// Replay every entry currently held for `source`, in emit
	/// order. Returns an empty vec for unknown sources rather
	/// than `None` — the panel renders an empty pane the same
	/// way either way.
	pub fn snapshot(&self, source: &str) -> Vec<LogEntry> {
		let buf = self.inner.lock().expect("log sink mutex poisoned");
		buf.get(source).map(|v| v.iter().cloned().collect()).unwrap_or_default()
	}

	/// Every source key that has at least one entry. The picker
	/// uses this to populate the popover; the order isn't
	/// stable across calls (HashMap iteration) so the frontend
	/// sorts before rendering.
	pub fn sources(&self) -> Vec<String> {
		self
			.inner
			.lock()
			.expect("log sink mutex poisoned")
			.keys()
			.cloned()
			.collect()
	}

	/// Drop every entry for `source`. The next emit re-creates
	/// the bucket. Used by the panel's `Clear` button.
	pub fn clear(&self, source: &str) {
		self.inner.lock().expect("log sink mutex poisoned").remove(source);
	}
}

fn now_ms() -> u64 {
	SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.map(|d| d.as_millis() as u64)
		.unwrap_or(0)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn emit_pushes_into_ring_and_assigns_monotonic_seq() {
		let sink = LogSink::new();
		sink.info("test", "first");
		sink.warn("test", "second");
		let entries = sink.snapshot("test");
		assert_eq!(entries.len(), 2);
		assert!(entries[0].seq < entries[1].seq);
		assert_eq!(entries[0].message, "first");
		assert_eq!(entries[1].level, LogLevel::Warn);
	}

	#[test]
	fn ring_trims_oldest_past_cap() {
		let sink = LogSink::new();
		for i in 0..(MAX_PER_SOURCE + 50) {
			sink.debug("ring", format!("{i}"));
		}
		let entries = sink.snapshot("ring");
		assert_eq!(entries.len(), MAX_PER_SOURCE);
		// First surviving entry should be the 51st emit
		// (we discarded the first 50 to make room).
		assert_eq!(entries[0].message, "50");
	}

	#[test]
	fn subscribe_receives_live_entries() {
		let sink = LogSink::new();
		let mut rx = sink.subscribe();
		sink.info("live", "hello");
		let entry = rx.try_recv().expect("entry should be queued");
		assert_eq!(entry.source, "live");
		assert_eq!(entry.message, "hello");
	}

	#[test]
	fn clear_drops_source_bucket() {
		let sink = LogSink::new();
		sink.info("a", "one");
		sink.info("b", "two");
		sink.clear("a");
		assert!(sink.snapshot("a").is_empty());
		assert_eq!(sink.snapshot("b").len(), 1);
	}

	#[test]
	fn sources_enumerates_buckets_with_entries() {
		let sink = LogSink::new();
		sink.info("alpha", "x");
		sink.info("beta", "y");
		let mut sources = sink.sources();
		sources.sort();
		assert_eq!(sources, vec!["alpha".to_owned(), "beta".to_owned()]);
	}
}
