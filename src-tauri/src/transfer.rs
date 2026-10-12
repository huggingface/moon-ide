//! File transfer between this desktop and the machine running the
//! workspace (Phase 16.6, ADR 0095). A local window copies on this
//! machine; a remote window streams through the server's SSH
//! connection. Either way the bytes land in a temp file next to the
//! destination that is renamed into place only once complete, so an
//! interrupted transfer never leaves a truncated file behind.

use std::collections::HashMap;
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use camino::{Utf8Path, Utf8PathBuf};
use moon_app::EventSink;
use moon_protocol::MoonError;
use serde::Serialize;
use tauri::{AppHandle, Manager, State};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::watch;

use crate::events::TauriEvents;
use crate::remote_workspace::{shell_quote, Ssh};
use crate::state::{AppState, Backend};

pub const PROGRESS_EVENT: &str = "transfer:progress";

const CHUNK_BYTES: usize = 256 * 1000;
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);
const UPLOAD_SUFFIX: &str = "moon-upload";
const DOWNLOAD_SUFFIX: &str = "moon-download";
const STDERR_LIMIT: u64 = 4000;

/// Cancel switches of the transfers in flight, by id.
#[derive(Default)]
pub struct Transfers {
	cancels: Mutex<HashMap<String, watch::Sender<bool>>>,
}

impl Transfers {
	fn register(&self) -> (String, watch::Receiver<bool>) {
		let id = uuid::Uuid::new_v4().to_string();
		let (tx, rx) = watch::channel(false);
		self.cancels.lock().expect("transfers poisoned").insert(id.clone(), tx);
		(id, rx)
	}

	fn remove(&self, id: &str) {
		self.cancels.lock().expect("transfers poisoned").remove(id);
	}

	fn cancel(&self, id: &str) {
		if let Some(tx) = self.cancels.lock().expect("transfers poisoned").get(id) {
			let _ = tx.send(true);
		}
	}
}

#[derive(Clone)]
enum Machine {
	Local,
	Remote(Ssh),
}

impl Machine {
	fn of(state: &AppState) -> Self {
		match &state.backend {
			Backend::Local(_) => Machine::Local,
			Backend::Remote(remote) => Machine::Remote(remote.ssh.clone()),
		}
	}
}

struct UploadFile {
	source: String,
	name: String,
	size: u64,
}

/// Copy desktop files into `target_dir` on the workspace's machine,
/// keeping their names. Returns the transfer id progress events carry.
#[tauri::command]
pub async fn transfer_upload(
	app: AppHandle,
	state: State<'_, AppState>,
	transfers: State<'_, Transfers>,
	sources: Vec<String>,
	target_dir: String,
) -> Result<String, MoonError> {
	if sources.is_empty() {
		return Err(MoonError::invalid("nothing to upload"));
	}
	if !target_dir.starts_with('/') {
		return Err(MoonError::invalid(format!(
			"upload target `{target_dir}` is not absolute"
		)));
	}
	let mut files = Vec::with_capacity(sources.len());
	for source in sources {
		let meta = tokio::fs::metadata(&source)
			.await
			.map_err(|err| MoonError::io(format!("{source}: {err}")))?;
		if meta.is_dir() {
			return Err(MoonError::invalid(format!(
				"{source} is a folder; only files can be uploaded"
			)));
		}
		if !meta.is_file() {
			return Err(MoonError::invalid(format!("{source} is not a regular file")));
		}
		let name = basename(&source)?.to_owned();
		files.push(UploadFile {
			source,
			name,
			size: meta.len(),
		});
	}
	let label = match files.as_slice() {
		[only] => only.name.clone(),
		_ => format!("{} files", files.len()),
	};
	let total = files.iter().map(|file| file.size).sum();
	let machine = Machine::of(&state);
	let target_dir = Utf8PathBuf::from(target_dir);
	let (id, mut cancel) = transfers.register();
	let mut progress = Progress::new(Arc::new(TauriEvents(app.clone())), id.clone(), label, total);
	tauri::async_runtime::spawn(async move {
		progress.emit(TransferState::Running, None);
		let mut outcome = Ok(());
		for file in &files {
			let dest = target_dir.join(&file.name);
			outcome = match &machine {
				Machine::Local => {
					copy_via_temp(
						Path::new(&file.source),
						&dest,
						UPLOAD_SUFFIX,
						&mut progress,
						&mut cancel,
					)
					.await
				}
				Machine::Remote(ssh) => remote_upload(ssh, &file.source, &dest, file.size, &mut progress, &mut cancel).await,
			};
			if outcome.is_err() {
				break;
			}
		}
		app.state::<Transfers>().remove(&progress.id);
		progress.finish(outcome);
	});
	Ok(id)
}

/// Copy one workspace file to `destination` on this desktop.
#[tauri::command]
pub async fn transfer_download(
	app: AppHandle,
	state: State<'_, AppState>,
	transfers: State<'_, Transfers>,
	source: String,
	destination: String,
) -> Result<String, MoonError> {
	if !source.starts_with('/') {
		return Err(MoonError::invalid(format!(
			"download source `{source}` is not absolute"
		)));
	}
	let label = basename(&source)?.to_owned();
	let dest = Utf8PathBuf::from(destination);
	basename(dest.as_str())?;
	let machine = Machine::of(&state);
	let (id, mut cancel) = transfers.register();
	let mut progress = Progress::new(Arc::new(TauriEvents(app.clone())), id.clone(), label, 0);
	tauri::async_runtime::spawn(async move {
		progress.emit(TransferState::Running, None);
		let outcome = match &machine {
			Machine::Local => match tokio::fs::metadata(&source).await {
				Ok(meta) if meta.is_file() => {
					progress.total = meta.len();
					copy_via_temp(Path::new(&source), &dest, DOWNLOAD_SUFFIX, &mut progress, &mut cancel).await
				}
				Ok(_) => Err(Failure::Error(format!("{source} is not a regular file"))),
				Err(err) => Err(Failure::Error(format!("{source}: {err}"))),
			},
			Machine::Remote(ssh) => remote_download(ssh, &source, &dest, &mut progress, &mut cancel).await,
		};
		app.state::<Transfers>().remove(&progress.id);
		progress.finish(outcome);
	});
	Ok(id)
}

#[tauri::command]
pub fn transfer_cancel(transfers: State<'_, Transfers>, id: String) {
	transfers.cancel(&id);
}

fn basename(path: &str) -> Result<&str, MoonError> {
	match Utf8Path::new(path).file_name() {
		Some(name) => Ok(name),
		None => Err(MoonError::invalid(format!("`{path}` has no file name"))),
	}
}

/// `.<name>.<suffix>` beside `dest`: same filesystem, so the final
/// rename is atomic, and hidden from a casual `ls`.
fn temp_sibling(dest: &Utf8Path, suffix: &str) -> Utf8PathBuf {
	let name = dest.file_name().unwrap_or("transfer");
	let temp = format!(".{name}.{suffix}");
	match dest.parent() {
		Some(parent) => parent.join(temp),
		None => Utf8PathBuf::from(temp),
	}
}

enum Failure {
	Cancelled,
	Error(String),
}

impl From<std::io::Error> for Failure {
	fn from(err: std::io::Error) -> Self {
		Failure::Error(err.to_string())
	}
}

type Outcome = Result<(), Failure>;

#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum TransferState {
	Running,
	Done,
	Failed,
	Cancelled,
}

#[derive(Serialize)]
struct ProgressPayload<'a> {
	id: &'a str,
	name: &'a str,
	bytes: u64,
	total: u64,
	state: TransferState,
	#[serde(skip_serializing_if = "Option::is_none")]
	error: Option<&'a str>,
}

/// Rate limit for `running` events: a multi-GB copy reads tens of
/// thousands of chunks.
struct Throttle {
	interval: Duration,
	last: Option<Instant>,
}

impl Throttle {
	fn new(interval: Duration) -> Self {
		Self { interval, last: None }
	}

	fn ready(&mut self, now: Instant) -> bool {
		if let Some(last) = self.last {
			if now.duration_since(last) < self.interval {
				return false;
			}
		}
		self.last = Some(now);
		true
	}
}

struct Progress {
	events: Arc<dyn EventSink>,
	id: String,
	name: String,
	bytes: u64,
	total: u64,
	throttle: Throttle,
}

impl Progress {
	fn new(events: Arc<dyn EventSink>, id: String, name: String, total: u64) -> Self {
		Self {
			events,
			id,
			name,
			bytes: 0,
			total,
			throttle: Throttle::new(PROGRESS_INTERVAL),
		}
	}

	fn advance(&mut self, bytes: u64) {
		self.bytes += bytes;
		if self.throttle.ready(Instant::now()) {
			self.emit(TransferState::Running, None);
		}
	}

	fn finish(self, outcome: Outcome) {
		match outcome {
			Ok(()) => self.emit(TransferState::Done, None),
			Err(Failure::Cancelled) => self.emit(TransferState::Cancelled, None),
			Err(Failure::Error(message)) => {
				tracing::warn!(transfer = %self.name, error = %message, "file transfer failed");
				self.emit(TransferState::Failed, Some(&message));
			}
		}
	}

	fn emit(&self, state: TransferState, error: Option<&str>) {
		let payload = ProgressPayload {
			id: &self.id,
			name: &self.name,
			bytes: self.bytes,
			total: self.total,
			state,
			error,
		};
		match serde_json::to_value(&payload) {
			Ok(value) => self.events.emit(PROGRESS_EVENT, value),
			Err(err) => tracing::warn!(error = %err, "could not serialise transfer progress"),
		}
	}
}

/// Resolves once the transfer is cancelled; never if the switch is
/// dropped without firing.
async fn cancelled(cancel: &mut watch::Receiver<bool>) {
	if cancel.wait_for(|cancelled| *cancelled).await.is_err() {
		std::future::pending::<()>().await;
	}
}

/// Chunked copy with progress; memory stays at one chunk whatever
/// the file size.
async fn pump<R, W>(
	reader: &mut R,
	writer: &mut W,
	progress: &mut Progress,
	cancel: &mut watch::Receiver<bool>,
) -> Result<u64, Failure>
where
	R: AsyncRead + Unpin,
	W: AsyncWrite + Unpin,
{
	let mut buf = vec![0u8; CHUNK_BYTES];
	let mut copied = 0u64;
	loop {
		let read = tokio::select! {
			biased;
			() = cancelled(cancel) => return Err(Failure::Cancelled),
			read = reader.read(&mut buf) => read?,
		};
		if read == 0 {
			break;
		}
		tokio::select! {
			biased;
			() = cancelled(cancel) => return Err(Failure::Cancelled),
			written = writer.write_all(&buf[..read]) => written?,
		}
		copied += read as u64;
		progress.advance(read as u64);
	}
	writer.flush().await?;
	Ok(copied)
}

async fn copy_via_temp(
	source: &Path,
	dest: &Utf8Path,
	suffix: &str,
	progress: &mut Progress,
	cancel: &mut watch::Receiver<bool>,
) -> Outcome {
	let temp = temp_sibling(dest, suffix);
	let outcome = async {
		let mut reader = tokio::fs::File::open(source).await?;
		let mut writer = tokio::fs::File::create(&temp).await?;
		pump(&mut reader, &mut writer, progress, cancel).await?;
		writer.sync_all().await?;
		drop(writer);
		tokio::fs::rename(&temp, dest)
			.await
			.map_err(|err| Failure::Error(format!("{dest}: {err}")))
	}
	.await;
	if outcome.is_err() {
		let _ = tokio::fs::remove_file(&temp).await;
	}
	outcome
}

/// Remote side of an upload: refuse to replace a directory, write
/// stdin to the temp file, and only rename it into place when the
/// byte count matches. A dropped connection closes stdin early, so
/// the size check is what keeps a truncated file out of `dest`.
fn upload_command(temp: &Utf8Path, dest: &Utf8Path, size: u64) -> String {
	let script = concat!(
		r#"if [ -d "$2" ]; then echo "$2 is a directory" >&2; exit 1; fi; "#,
		r#"cat > "$1" || { rm -f "$1"; exit 1; }; "#,
		r#"[ $(wc -c < "$1") -eq "$3" ] || { rm -f "$1"; echo "upload incomplete" >&2; exit 1; }; "#,
		r#"mv -f "$1" "$2" || { rm -f "$1"; exit 1; }"#,
	);
	format!(
		"sh -c {} sh {} {} {size}",
		shell_quote(script),
		shell_quote(temp.as_str()),
		shell_quote(dest.as_str())
	)
}

/// Remote side of a download: the size on the first line, then the
/// raw bytes.
fn download_command(source: &str) -> String {
	let script =
		r#"if [ ! -f "$1" ]; then echo "$1 is not a regular file" >&2; exit 1; fi; wc -c < "$1" && exec cat "$1""#;
	format!("sh -c {} sh {}", shell_quote(script), shell_quote(source))
}

fn parse_size(line: &str) -> Option<u64> {
	line.trim().parse().ok()
}

fn remove_command(path: &Utf8Path) -> String {
	format!("rm -f {}", shell_quote(path.as_str()))
}

async fn remote_upload(
	ssh: &Ssh,
	source: &str,
	dest: &Utf8Path,
	size: u64,
	progress: &mut Progress,
	cancel: &mut watch::Receiver<bool>,
) -> Outcome {
	let temp = temp_sibling(dest, UPLOAD_SUFFIX);
	let mut child = ssh
		.command()
		.arg(upload_command(&temp, dest, size))
		.stdin(Stdio::piped())
		.stdout(Stdio::null())
		.stderr(Stdio::piped())
		.spawn()?;
	let stderr = collect_stderr(child.stderr.take());
	let outcome = async {
		let mut stdin = child
			.stdin
			.take()
			.ok_or_else(|| Failure::Error("ssh has no stdin".to_owned()))?;
		let mut reader = tokio::fs::File::open(source).await?;
		let pumped = pump(&mut reader, &mut stdin, progress, cancel).await;
		drop(stdin);
		match pumped {
			Err(Failure::Cancelled) => Err(Failure::Cancelled),
			// Usually a broken pipe because the remote side gave up;
			// its stderr says why.
			Err(Failure::Error(local)) => match wait_child(&mut child, cancel).await {
				Ok(status) => Err(Failure::Error(remote_error(status, stderr.await, Some(local)))),
				Err(failure) => Err(failure),
			},
			Ok(_) => {
				let status = wait_child(&mut child, cancel).await?;
				if !status.success() {
					return Err(Failure::Error(remote_error(status, stderr.await, None)));
				}
				Ok(())
			}
		}
	}
	.await;
	if outcome.is_err() {
		let _ = child.kill().await;
		remove_remote(ssh, &temp).await;
	}
	outcome
}

async fn remote_download(
	ssh: &Ssh,
	source: &str,
	dest: &Utf8Path,
	progress: &mut Progress,
	cancel: &mut watch::Receiver<bool>,
) -> Outcome {
	let mut child = ssh
		.command()
		.arg(download_command(source))
		.stdin(Stdio::null())
		.stdout(Stdio::piped())
		.stderr(Stdio::piped())
		.spawn()?;
	let stderr = collect_stderr(child.stderr.take());
	let Some(stdout) = child.stdout.take() else {
		return Err(Failure::Error("ssh has no stdout".to_owned()));
	};
	let mut stdout = tokio::io::BufReader::new(stdout);
	let mut header = String::new();
	tokio::select! {
		biased;
		() = cancelled(cancel) => return Err(Failure::Cancelled),
		read = stdout.read_line(&mut header) => { read?; }
	}
	let Some(total) = parse_size(&header) else {
		let status = wait_child(&mut child, cancel).await?;
		return Err(Failure::Error(remote_error(status, stderr.await, None)));
	};
	progress.total = total;
	progress.emit(TransferState::Running, None);
	let temp = temp_sibling(dest, DOWNLOAD_SUFFIX);
	let outcome = async {
		let mut writer = tokio::fs::File::create(&temp).await?;
		let copied = pump(&mut stdout, &mut writer, progress, cancel).await?;
		writer.sync_all().await?;
		drop(writer);
		let status = wait_child(&mut child, cancel).await?;
		if !status.success() {
			return Err(Failure::Error(remote_error(status, stderr.await, None)));
		}
		if copied != total {
			return Err(Failure::Error(format!(
				"download incomplete: {copied} of {total} bytes"
			)));
		}
		tokio::fs::rename(&temp, dest)
			.await
			.map_err(|err| Failure::Error(format!("{dest}: {err}")))
	}
	.await;
	if outcome.is_err() {
		let _ = child.kill().await;
		let _ = tokio::fs::remove_file(&temp).await;
	}
	outcome
}

async fn wait_child(
	child: &mut tokio::process::Child,
	cancel: &mut watch::Receiver<bool>,
) -> Result<ExitStatus, Failure> {
	tokio::select! {
		biased;
		() = cancelled(cancel) => Err(Failure::Cancelled),
		status = child.wait() => Ok(status?),
	}
}

/// Best effort: the upload script already removes its temp file when
/// stdin closes early, but not if the remote shell itself was killed.
async fn remove_remote(ssh: &Ssh, path: &Utf8Path) {
	let result = ssh
		.command()
		.arg(remove_command(path))
		.stdin(Stdio::null())
		.stdout(Stdio::null())
		.stderr(Stdio::null())
		.status()
		.await;
	if let Err(err) = result {
		tracing::warn!(error = %err, path = %path, "could not remove remote upload temp file");
	}
}

/// Drains the child's stderr concurrently (so a chatty remote can't
/// block on a full pipe) and keeps the head for error messages.
fn collect_stderr(stderr: Option<tokio::process::ChildStderr>) -> impl std::future::Future<Output = String> {
	let task = tauri::async_runtime::spawn(async move {
		let Some(mut stderr) = stderr else {
			return String::new();
		};
		let mut head = Vec::new();
		let _ = (&mut stderr).take(STDERR_LIMIT).read_to_end(&mut head).await;
		let _ = tokio::io::copy(&mut stderr, &mut tokio::io::sink()).await;
		String::from_utf8_lossy(&head).trim().to_owned()
	});
	async move { task.await.unwrap_or_default() }
}

fn remote_error(status: ExitStatus, stderr: String, local: Option<String>) -> String {
	if !stderr.is_empty() {
		return stderr;
	}
	if let Some(local) = local {
		return local;
	}
	format!("remote command failed ({status})")
}

#[cfg(test)]
mod tests {
	use super::*;
	use serde_json::Value;

	#[derive(Default)]
	struct Recorder(Mutex<Vec<Value>>);

	impl EventSink for Recorder {
		fn emit(&self, event: &str, payload: Value) {
			assert_eq!(event, PROGRESS_EVENT);
			self.0.lock().unwrap().push(payload);
		}
	}

	fn progress(recorder: &Arc<Recorder>) -> Progress {
		Progress::new(recorder.clone(), "t1".to_owned(), "a.bin".to_owned(), 0)
	}

	#[test]
	fn temp_sibling_hides_next_to_dest() {
		assert_eq!(
			temp_sibling(Utf8Path::new("/srv/ws/data.tar"), UPLOAD_SUFFIX),
			Utf8PathBuf::from("/srv/ws/.data.tar.moon-upload")
		);
		assert_eq!(
			temp_sibling(Utf8Path::new("/home/me/Downloads/x"), DOWNLOAD_SUFFIX),
			Utf8PathBuf::from("/home/me/Downloads/.x.moon-download")
		);
	}

	#[test]
	fn upload_command_quotes_every_path() {
		let command = upload_command(
			Utf8Path::new("/srv/it's/.a b.moon-upload"),
			Utf8Path::new("/srv/it's/a b"),
			42,
		);
		assert!(command.starts_with("sh -c 'if [ -d \"$2\" ]"));
		assert!(command.ends_with(" sh '/srv/it'\\''s/.a b.moon-upload' '/srv/it'\\''s/a b' 42"));
	}

	#[test]
	fn download_and_remove_commands_quote_the_path() {
		assert!(download_command("/srv/$HOME/f").ends_with(" sh '/srv/$HOME/f'"));
		assert_eq!(
			remove_command(Utf8Path::new("/srv/.f.moon-upload")),
			"rm -f '/srv/.f.moon-upload'"
		);
	}

	#[test]
	fn parse_size_tolerates_bsd_wc_padding() {
		assert_eq!(parse_size("     1234\n"), Some(1234));
		assert_eq!(parse_size("0\n"), Some(0));
		assert_eq!(parse_size(""), None);
		assert_eq!(parse_size("wc: nope\n"), None);
	}

	#[test]
	fn throttle_allows_one_event_per_interval() {
		let mut throttle = Throttle::new(Duration::from_millis(100));
		let start = Instant::now();
		assert!(throttle.ready(start));
		assert!(!throttle.ready(start + Duration::from_millis(50)));
		assert!(throttle.ready(start + Duration::from_millis(100)));
		assert!(!throttle.ready(start + Duration::from_millis(199)));
		assert!(throttle.ready(start + Duration::from_millis(250)));
	}

	#[test]
	fn basename_rejects_paths_without_a_name() {
		assert_eq!(basename("/a/b.txt").unwrap(), "b.txt");
		assert!(basename("/").is_err());
	}

	#[tokio::test]
	async fn copy_via_temp_renames_into_place() {
		let dir = tempfile::tempdir().unwrap();
		let root = Utf8Path::from_path(dir.path()).unwrap();
		let source = root.join("source.bin");
		let data = vec![7u8; CHUNK_BYTES * 2 + 5];
		std::fs::write(&source, &data).unwrap();
		let dest = root.join("dest.bin");
		let recorder = Arc::new(Recorder::default());
		let mut progress = progress(&recorder);
		let (_tx, mut cancel) = watch::channel(false);
		let outcome = copy_via_temp(source.as_std_path(), &dest, UPLOAD_SUFFIX, &mut progress, &mut cancel).await;
		assert!(outcome.is_ok());
		assert_eq!(std::fs::read(&dest).unwrap(), data);
		assert!(!temp_sibling(&dest, UPLOAD_SUFFIX).exists());
		assert_eq!(progress.bytes, data.len() as u64);
		assert!(!recorder.0.lock().unwrap().is_empty());
	}

	#[tokio::test]
	async fn cancelled_copy_leaves_no_files() {
		let dir = tempfile::tempdir().unwrap();
		let root = Utf8Path::from_path(dir.path()).unwrap();
		let source = root.join("source.bin");
		std::fs::write(&source, vec![1u8; CHUNK_BYTES * 3]).unwrap();
		let dest = root.join("dest.bin");
		let recorder = Arc::new(Recorder::default());
		let mut progress = progress(&recorder);
		let (tx, mut cancel) = watch::channel(false);
		tx.send(true).unwrap();
		let outcome = copy_via_temp(source.as_std_path(), &dest, UPLOAD_SUFFIX, &mut progress, &mut cancel).await;
		assert!(matches!(outcome, Err(Failure::Cancelled)));
		assert!(!dest.exists());
		assert!(!temp_sibling(&dest, UPLOAD_SUFFIX).exists());
	}

	#[test]
	fn finish_reports_failure_with_error() {
		let recorder = Arc::new(Recorder::default());
		progress(&recorder).finish(Err(Failure::Error("disk full".to_owned())));
		let events = recorder.0.lock().unwrap();
		assert_eq!(events[0]["state"], "failed");
		assert_eq!(events[0]["error"], "disk full");
		assert_eq!(events[0]["id"], "t1");
	}
}
