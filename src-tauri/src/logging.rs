//! Process logging: stdout, plus a daily-rotated file per workspace.
//!
//! A desktop launch has no terminal, so stdout goes nowhere and a
//! problem that happened across a quit and relaunch (the container
//! lifecycle, mostly) left nothing to read. The file is what makes
//! those diagnosable after the fact.

use camino::Utf8Path;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::{FormatTime, SystemTime};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

/// Days of log files kept per workspace.
const KEPT_FILES: usize = 7;

/// Timestamp followed by the process id. A relaunch overlaps the
/// previous instance's shutdown and both append to the same file; the
/// id is what tells their lines apart.
struct TimeAndPid(u32);

impl FormatTime for TimeAndPid {
	fn format_time(&self, w: &mut Writer<'_>) -> std::fmt::Result {
		SystemTime.format_time(w)?;
		write!(w, " [{}]", self.0)
	}
}

fn filter() -> EnvFilter {
	EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,moon=debug"))
}

/// The file half: `<logs_dir>/<name>.<date>.log`, one per day, the
/// oldest dropped past [`KEPT_FILES`]. `None` when the directory
/// can't be used; logging then goes to stdout alone.
///
/// Writes are synchronous: the lines that matter most are the last
/// ones before `exit`, which a background writer would drop.
fn file_layer<S>(logs_dir: &Utf8Path, name: &str) -> Option<impl Layer<S>>
where
	S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
	// The name comes from the command line; keep it one path segment.
	let prefix: String = name
		.chars()
		.map(|c| {
			if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
				c
			} else {
				'_'
			}
		})
		.collect();
	let appender = RollingFileAppender::builder()
		.rotation(Rotation::DAILY)
		.filename_prefix(prefix)
		.filename_suffix("log")
		.max_log_files(KEPT_FILES)
		.build(logs_dir)
		.map_err(|err| eprintln!("moon-ide: no log file in {logs_dir}: {err}"))
		.ok()?;
	Some(
		tracing_subscriber::fmt::layer()
			.with_ansi(false)
			.with_timer(TimeAndPid(std::process::id()))
			.with_writer(appender)
			.with_filter(filter()),
	)
}

/// Install the global subscriber: stdout as before, plus the file
/// named after `name` (the workspace slug, so two workspaces' windows
/// don't interleave) when there is a `logs_dir`.
pub fn init(logs_dir: Option<&Utf8Path>, name: &str) {
	tracing_subscriber::registry()
		.with(tracing_subscriber::fmt::layer().with_filter(filter()))
		.with(logs_dir.and_then(|dir| file_layer(dir, name)))
		.init();
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn events_land_in_a_dated_file_with_the_process_id() {
		let dir = tempfile::tempdir().unwrap();
		let logs = Utf8Path::from_path(dir.path()).unwrap().join("logs");
		let subscriber = tracing_subscriber::registry().with(file_layer(&logs, "my/ws"));
		tracing::subscriber::with_default(subscriber, || {
			tracing::info!(folder = "/code/app", "stop_all: stopped workspace shell");
		});

		let files: Vec<_> = std::fs::read_dir(&logs)
			.unwrap()
			.map(|entry| entry.unwrap().path())
			.collect();
		assert_eq!(files.len(), 1, "{files:?}");
		let name = files[0].file_name().unwrap().to_str().unwrap();
		assert!(name.starts_with("my_ws.") && name.ends_with(".log"), "{name}");
		let text = std::fs::read_to_string(&files[0]).unwrap();
		assert!(text.contains(&format!(" [{}] ", std::process::id())), "{text}");
		assert!(
			text.contains("stop_all: stopped workspace shell") && text.contains("/code/app"),
			"{text}"
		);
		assert!(!text.contains('\u{1b}'), "no colour codes in the file: {text}");
	}
}
