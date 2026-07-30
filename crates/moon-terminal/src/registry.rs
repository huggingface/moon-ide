//! Registry of live terminal sessions, with enough state that
//! something other than the terminal's own tab can inspect them.
//!
//! The coder's `list_terminals` / `read_terminal` tools are the
//! only consumer today: an agent working in a project can see the
//! terminals the user opened for that project and read what they
//! printed, instead of re-running the command through `bash` and
//! guessing why the two disagree. See
//! [ADR 0048](../../../specs/decisions/0048-coder-reads-terminals.md).
//!
//! Two things are kept per terminal:
//!
//! - **Metadata** ([`TerminalInfo`]) — target, cwd, the bound
//!   folder the terminal was opened for, and liveness. The folder
//!   tag is what makes "only this project's terminals" answerable;
//!   it's passed in at open time rather than reverse-engineered
//!   from `cwd`, which would be ambiguous for worktrees (they ride
//!   the parent's bind mount, so their container cwd is a path
//!   *under* the parent's) and wrong the moment the user `cd`s.
//! - **A bounded ring of raw PTY bytes**, [`SCROLLBACK_BYTES`]
//!   deep. Escape sequences and all — the bytes are replayed
//!   through a throwaway [`vt100`] emulator at read time, which is
//!   what turns `\r`-redrawn progress bars, colour codes and
//!   cursor addressing back into the text the user is actually
//!   looking at. Emulating eagerly (one live `vt100::Parser` per
//!   terminal) would cost megabytes of standing grid per tab to
//!   serve a read that happens approximately never; a byte ring
//!   puts the cost on the rare read instead of the hot write path.
//!
//! Registry lifetime tracks the *tab*, not the child process: a
//! terminal whose shell exited stays readable (its output is still
//! on screen for the user, so it should still be answerable for the
//! agent) and is dropped only when the tab closes.

use std::collections::{HashMap, VecDeque};

use camino::{Utf8Path, Utf8PathBuf};
use tokio::sync::Mutex;

/// Raw PTY bytes retained per terminal. 256 kB is a few thousand
/// lines of ordinary command output — well past what an agent
/// should be pulling into context in one read, and small enough
/// that a fistful of open terminals is invisible next to the
/// editor's own buffers. The frontend's xterm.js keeps its own
/// (larger, 5000-line) scrollback; the two are independent, so a
/// read can legitimately reach less far back than what the user
/// can scroll to.
pub const SCROLLBACK_BYTES: usize = 256_000;

/// Default number of rendered lines [`TerminalRegistry::read`]
/// returns when the caller doesn't ask for a specific count.
pub const DEFAULT_READ_LINES: usize = 200;

/// Hard cap on rendered lines per read, so one call can't dump an
/// entire dev-server log into an LLM context window.
pub const MAX_READ_LINES: usize = 2_000;

/// Cap on the rendered text of a single read. Long-line output
/// (minified bundles, base64 blobs) can blow past a line budget
/// by orders of magnitude, so the character budget is enforced
/// independently, keeping the *tail*.
pub const MAX_READ_CHARS: usize = 100_000;

/// Where a registered terminal's shell runs. Mirrors the
/// `host` / `container` split of [`crate::TerminalTarget`], minus
/// the spawn details — consumers only need to tell the user (or the
/// model) which side of the boundary the terminal is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalKind {
	Host,
	Container,
}

impl TerminalKind {
	pub fn as_str(self) -> &'static str {
		match self {
			Self::Host => "host",
			Self::Container => "container",
		}
	}
}

/// What the terminal commands hand the registry at open time.
#[derive(Debug, Clone)]
pub struct TerminalRegistration {
	pub kind: TerminalKind,
	/// Working directory the shell started in, in the target's own
	/// path space (a host path for `Host`, an in-container path for
	/// `Container`).
	pub cwd: String,
	/// Absolute host path of the bound workspace folder this
	/// terminal was opened for. `None` for terminals with no
	/// project (a `$HOME` shell in a folder-less workspace); those
	/// never match a folder-scoped listing.
	pub folder: Option<Utf8PathBuf>,
	pub cols: u16,
	pub rows: u16,
}

/// Snapshot of one registered terminal. Cloned out of the registry
/// so callers never render while holding the lock.
#[derive(Debug, Clone)]
pub struct TerminalInfo {
	pub id: String,
	pub kind: TerminalKind,
	pub cwd: String,
	pub folder: Option<Utf8PathBuf>,
	pub cols: u16,
	pub rows: u16,
	/// `false` once the child has exited; the tab (and its
	/// scrollback) is still around.
	pub running: bool,
	/// Exit code, when the child surfaced one.
	pub exit_code: Option<i32>,
	/// Bytes currently retained in the ring — a rough "how much is
	/// there to read" hint, capped by [`SCROLLBACK_BYTES`].
	pub buffered_bytes: usize,
}

/// Rendered read of one terminal: its metadata plus the tail of
/// its output as plain text.
#[derive(Debug, Clone)]
pub struct TerminalRead {
	pub info: TerminalInfo,
	pub lines: Vec<String>,
	/// `true` when output was dropped from the front of the
	/// rendered tail — either the line budget or
	/// [`MAX_READ_CHARS`] bit, or the ring itself had already
	/// discarded older bytes.
	pub truncated: bool,
}

struct Entry {
	info: TerminalInfo,
	/// Raw PTY bytes, oldest first, capped at
	/// [`SCROLLBACK_BYTES`].
	ring: VecDeque<u8>,
	/// Whether the ring has ever dropped bytes off its front.
	dropped: bool,
}

/// Shared map of open terminals. Cheap to clone (`Arc` it once at
/// startup); every method takes the lock briefly and gets out.
#[derive(Default)]
pub struct TerminalRegistry {
	entries: Mutex<HashMap<String, Entry>>,
}

impl TerminalRegistry {
	/// Record a newly-opened terminal. Called by `terminal_open`
	/// right after the PTY spawns.
	pub async fn register(&self, id: &str, registration: TerminalRegistration) {
		let TerminalRegistration {
			kind,
			cwd,
			folder,
			cols,
			rows,
		} = registration;
		let entry = Entry {
			info: TerminalInfo {
				id: id.to_owned(),
				kind,
				cwd,
				folder,
				cols,
				rows,
				running: true,
				exit_code: None,
				buffered_bytes: 0,
			},
			ring: VecDeque::new(),
			dropped: false,
		};
		self.entries.lock().await.insert(id.to_owned(), entry);
	}

	/// Append a chunk of PTY output. Hot path: one `extend` plus a
	/// front-drain when over budget.
	pub async fn record_output(&self, id: &str, bytes: &[u8]) {
		let mut entries = self.entries.lock().await;
		let Some(entry) = entries.get_mut(id) else {
			return;
		};
		entry.ring.extend(bytes.iter().copied());
		if entry.ring.len() > SCROLLBACK_BYTES {
			let excess = entry.ring.len() - SCROLLBACK_BYTES;
			entry.ring.drain(..excess);
			entry.dropped = true;
		}
		entry.info.buffered_bytes = entry.ring.len();
	}

	/// Track a resize so reads render at the width the user is
	/// currently looking at.
	pub async fn record_resize(&self, id: &str, cols: u16, rows: u16) {
		let mut entries = self.entries.lock().await;
		if let Some(entry) = entries.get_mut(id) {
			entry.info.cols = cols;
			entry.info.rows = rows;
		}
	}

	/// Mark the child as exited, keeping the entry (and its
	/// scrollback) readable until the tab closes.
	pub async fn mark_exited(&self, id: &str, exit_code: Option<i32>) {
		let mut entries = self.entries.lock().await;
		if let Some(entry) = entries.get_mut(id) {
			entry.info.running = false;
			entry.info.exit_code = exit_code;
		}
	}

	/// Drop a terminal entirely — the tab is gone, so its output
	/// is no longer something the user can see either.
	pub async fn forget(&self, id: &str) {
		self.entries.lock().await.remove(id);
	}

	/// Every terminal opened for `folder`, oldest-registered
	/// first. Terminals belonging to another bound folder (or to
	/// none) are omitted: a session works one project, and a
	/// worktree session is its own project.
	pub async fn list_for_folder(&self, folder: &Utf8Path) -> Vec<TerminalInfo> {
		let entries = self.entries.lock().await;
		let mut out: Vec<TerminalInfo> = entries
			.values()
			.filter(|entry| entry.info.folder.as_deref() == Some(folder))
			.map(|entry| entry.info.clone())
			.collect();
		// `HashMap` iteration order is arbitrary; sort by id so
		// repeated listings are stable (ids are UUIDs, so this is
		// arbitrary-but-stable rather than chronological).
		out.sort_by(|a, b| a.id.cmp(&b.id));
		out
	}

	/// Metadata for one terminal, without rendering its output.
	pub async fn info(&self, id: &str) -> Option<TerminalInfo> {
		self.entries.lock().await.get(id).map(|entry| entry.info.clone())
	}

	/// Render the tail of a terminal's output as plain text.
	///
	/// `max_lines` is clamped to [`MAX_READ_LINES`] and defaults to
	/// [`DEFAULT_READ_LINES`]. The retained bytes are copied out
	/// under the lock and emulated outside it, so a big read never
	/// stalls the supervisors writing into other terminals.
	pub async fn read(&self, id: &str, max_lines: Option<usize>) -> Option<TerminalRead> {
		let (info, bytes, dropped) = {
			let entries = self.entries.lock().await;
			let entry = entries.get(id)?;
			let mut bytes = Vec::with_capacity(entry.ring.len());
			let (head, tail) = entry.ring.as_slices();
			bytes.extend_from_slice(head);
			bytes.extend_from_slice(tail);
			(entry.info.clone(), bytes, entry.dropped)
		};
		let limit = max_lines.unwrap_or(DEFAULT_READ_LINES).clamp(1, MAX_READ_LINES);
		let rendered = render_tail(&bytes, info.cols, info.rows, limit);
		let mut truncated = dropped || rendered.clipped;
		let lines = clamp_chars(rendered.lines, &mut truncated);
		Some(TerminalRead { info, lines, truncated })
	}
}

/// Outcome of replaying a byte ring through the emulator.
struct RenderedTail {
	lines: Vec<String>,
	/// `true` when there were more lines above what we returned.
	clipped: bool,
}

/// Replay `bytes` through a throwaway [`vt100`] emulator sized to
/// the terminal, and return the last `max_lines` non-empty-trailing
/// rows as plain text.
///
/// Replaying a *truncated* byte stream can start mid-escape
/// sequence; the parser resyncs on the next one, so at worst the
/// oldest line is cosmetically mangled. Bytes are always replayed
/// at the terminal's current width, which is what the user sees now
/// but not necessarily how older output was wrapped when it was
/// printed.
fn render_tail(bytes: &[u8], cols: u16, rows: u16, max_lines: usize) -> RenderedTail {
	let cols = cols.max(1);
	let rows = rows.max(1);
	let page_rows = usize::from(rows);
	// Over-fetch by a page: the bottom screen is padded with blank
	// rows on a terminal that hasn't filled it, and those get
	// trimmed — so "did we collect more than the caller asked for"
	// is only a truthful truncation signal with a page of slack.
	// The emulator keeps one page beyond that, so the over-fetch
	// finds real content whenever there is any.
	let fetch_target = max_lines.saturating_add(page_rows);
	let scrollback = fetch_target.saturating_add(page_rows);
	let mut parser = vt100::Parser::new(rows, cols, scrollback);
	parser.process(bytes);

	// Walk up the scrollback one screenful at a time, prepending
	// each page. `set_scrollback` clamps to the scrollback that
	// actually exists, so the *reported* offset after each set is
	// what tells us how much new content the page carries — and
	// when we've hit the top.
	let screen = parser.screen_mut();
	let mut lines: VecDeque<String> = VecDeque::with_capacity(fetch_target);
	let mut requested = 0usize;
	let mut previous = 0usize;
	// Whether the walk ran out of scrollback rather than stopping
	// on its own fetch budget. Stopping on the budget means there
	// was still content above.
	let mut reached_top = false;
	loop {
		screen.set_scrollback(requested);
		let actual = screen.scrollback();
		let page: Vec<String> = screen.rows(0, cols).collect();
		let fresh = if requested == 0 {
			// Bottom page: everything on it is new.
			page_rows
		} else {
			// Clamped or not, the page only contributes the rows
			// between the previous offset and this one.
			actual.saturating_sub(previous)
		};
		if fresh == 0 {
			reached_top = true;
			break;
		}
		for line in page.into_iter().take(fresh).rev() {
			lines.push_front(line);
		}
		if actual < requested {
			// The offset got clamped: that was the last page.
			reached_top = true;
			break;
		}
		if lines.len() >= fetch_target {
			break;
		}
		previous = actual;
		requested += page_rows;
	}

	// The bottom page is mostly empty rows on a terminal that has
	// printed less than a screenful, and TUIs pad to the full
	// height. Trailing blanks carry no information either way.
	while lines.back().is_some_and(|line| line.trim().is_empty()) {
		lines.pop_back();
	}
	while lines.front().is_some_and(|line| line.trim().is_empty()) {
		lines.pop_front();
	}
	let mut clipped = !reached_top;
	while lines.len() > max_lines {
		lines.pop_front();
		clipped = true;
	}
	RenderedTail {
		lines: lines.into(),
		clipped,
	}
}

/// Enforce [`MAX_READ_CHARS`] over the rendered tail, dropping
/// whole lines from the front so the newest output survives.
fn clamp_chars(lines: Vec<String>, truncated: &mut bool) -> Vec<String> {
	let total: usize = lines.iter().map(|line| line.chars().count() + 1).sum();
	if total <= MAX_READ_CHARS {
		return lines;
	}
	*truncated = true;
	let mut kept: VecDeque<String> = VecDeque::new();
	let mut budget = MAX_READ_CHARS;
	for line in lines.into_iter().rev() {
		let cost = line.chars().count() + 1;
		if cost > budget {
			break;
		}
		budget -= cost;
		kept.push_front(line);
	}
	kept.into()
}

#[cfg(test)]
mod tests {
	use super::*;

	fn registration() -> TerminalRegistration {
		TerminalRegistration {
			kind: TerminalKind::Host,
			cwd: "/home/dev/code/moon-ide".into(),
			folder: Some(Utf8PathBuf::from("/home/dev/code/moon-ide")),
			cols: 80,
			rows: 24,
		}
	}

	#[tokio::test]
	async fn reads_back_plain_output() {
		let registry = TerminalRegistry::default();
		registry.register("t1", registration()).await;
		registry.record_output("t1", b"hello\r\nworld\r\n").await;
		let read = registry.read("t1", None).await.expect("registered");
		assert_eq!(read.lines, vec!["hello".to_string(), "world".to_string()]);
		assert!(read.info.running);
		assert!(!read.truncated);
	}

	/// The whole reason output is emulated instead of
	/// ANSI-stripped: a progress bar redrawing one line with `\r`
	/// (plus the erase-to-end-of-line every real one emits) must
	/// read back as its final state, not as every frame it ever
	/// painted.
	#[tokio::test]
	async fn collapses_carriage_return_redraws() {
		let registry = TerminalRegistry::default();
		registry.register("t1", registration()).await;
		registry
			.record_output(
				"t1",
				b"Building [=>    ] 20%\r\x1b[KBuilding [====>] 99%\r\x1b[KDone!\r\n",
			)
			.await;
		let read = registry.read("t1", None).await.expect("registered");
		assert_eq!(read.lines, vec!["Done!".to_string()]);
	}

	#[tokio::test]
	async fn strips_colour_codes() {
		let registry = TerminalRegistry::default();
		registry.register("t1", registration()).await;
		registry
			.record_output("t1", b"\x1b[32mtest result: ok\x1b[0m\r\n")
			.await;
		let read = registry.read("t1", None).await.expect("registered");
		assert_eq!(read.lines, vec!["test result: ok".to_string()]);
	}

	/// Output that fits inside the budget isn't truncated, even
	/// though the emulator's bottom screen is padded with blank rows
	/// that the renderer trims away.
	#[tokio::test]
	async fn output_within_budget_is_not_flagged_truncated() {
		let registry = TerminalRegistry::default();
		registry.register("t1", registration()).await;
		let mut out = String::new();
		for i in 1..=30 {
			out.push_str(&format!("line {i}\r\n"));
		}
		registry.record_output("t1", out.as_bytes()).await;
		let read = registry.read("t1", Some(30)).await.expect("registered");
		assert_eq!(read.lines.len(), 30);
		assert!(!read.truncated);
	}

	#[tokio::test]
	async fn returns_the_tail_when_more_lines_exist_than_asked_for() {
		let registry = TerminalRegistry::default();
		registry.register("t1", registration()).await;
		let mut out = String::new();
		for i in 1..=120 {
			out.push_str(&format!("line {i}\r\n"));
		}
		registry.record_output("t1", out.as_bytes()).await;
		let read = registry.read("t1", Some(10)).await.expect("registered");
		assert_eq!(read.lines.len(), 10);
		assert_eq!(read.lines.first().map(String::as_str), Some("line 111"));
		assert_eq!(read.lines.last().map(String::as_str), Some("line 120"));
		assert!(read.truncated);
	}

	/// Output spanning more than one screenful has to come back
	/// in order — the reader walks the emulator's scrollback a
	/// page at a time, so an off-by-one page would silently
	/// duplicate or drop rows.
	#[tokio::test]
	async fn walks_multiple_scrollback_pages_in_order() {
		let registry = TerminalRegistry::default();
		registry.register("t1", registration()).await;
		let mut out = String::new();
		for i in 1..=100 {
			out.push_str(&format!("line {i}\r\n"));
		}
		registry.record_output("t1", out.as_bytes()).await;
		let read = registry.read("t1", Some(100)).await.expect("registered");
		let expected: Vec<String> = (1..=100).map(|i| format!("line {i}")).collect();
		assert_eq!(read.lines, expected);
	}

	#[tokio::test]
	async fn ring_drops_oldest_bytes_and_flags_truncation() {
		let registry = TerminalRegistry::default();
		registry.register("t1", registration()).await;
		let chunk = vec![b'x'; SCROLLBACK_BYTES];
		registry.record_output("t1", &chunk).await;
		registry.record_output("t1", b"\r\ntail line\r\n").await;
		let read = registry.read("t1", Some(5)).await.expect("registered");
		assert_eq!(read.lines.last().map(String::as_str), Some("tail line"));
		assert!(read.truncated);
		assert_eq!(read.info.buffered_bytes, SCROLLBACK_BYTES);
	}

	#[tokio::test]
	async fn exited_terminals_stay_readable_until_forgotten() {
		let registry = TerminalRegistry::default();
		registry.register("t1", registration()).await;
		registry.record_output("t1", b"cargo test\r\nok\r\n").await;
		registry.mark_exited("t1", Some(0)).await;
		let read = registry.read("t1", None).await.expect("still registered");
		assert!(!read.info.running);
		assert_eq!(read.info.exit_code, Some(0));
		assert_eq!(read.lines.last().map(String::as_str), Some("ok"));
		registry.forget("t1").await;
		assert!(registry.read("t1", None).await.is_none());
	}

	#[tokio::test]
	async fn lists_only_the_requested_folder() {
		let registry = TerminalRegistry::default();
		registry.register("mine", registration()).await;
		registry
			.register(
				"sibling",
				TerminalRegistration {
					folder: Some(Utf8PathBuf::from("/home/dev/code/moon-landing")),
					..registration()
				},
			)
			.await;
		registry
			.register(
				"folderless",
				TerminalRegistration {
					folder: None,
					..registration()
				},
			)
			.await;
		let listed = registry.list_for_folder(Utf8Path::new("/home/dev/code/moon-ide")).await;
		assert_eq!(listed.len(), 1);
		assert_eq!(listed[0].id, "mine");
	}

	#[tokio::test]
	async fn long_lines_are_clamped_by_character_budget() {
		let registry = TerminalRegistry::default();
		registry.register("t1", registration()).await;
		// Wide terminal so one logical line stays one row.
		registry.record_resize("t1", 2000, 24).await;
		let mut out = String::new();
		for i in 0..200 {
			out.push_str(&"z".repeat(1900));
			out.push_str(&format!("{i}\r\n"));
		}
		registry.record_output("t1", out.as_bytes()).await;
		let read = registry.read("t1", Some(MAX_READ_LINES)).await.expect("registered");
		let chars: usize = read.lines.iter().map(|line| line.chars().count() + 1).sum();
		assert!(chars <= MAX_READ_CHARS, "rendered {chars} chars");
		assert!(read.truncated);
	}
}
