//! Microphone capture for the composer's voice input.
//!
//! Desktop-local on purpose: the microphone is on the machine running
//! the window, while the workspace process (which transcribes) may be
//! remote. Recording shells out to PipeWire's `pw-record` (falling back
//! to ALSA's `arecord`) rather than linking an audio library, so the
//! build needs no ALSA headers. Linux only for now.

use std::path::PathBuf;
use std::time::Duration;

use base64::Engine as _;
use moon_protocol::MoonError;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

/// 16 kHz mono 16-bit: what Whisper consumes, and small (32 kB/s).
const SAMPLE_RATE: &str = "16000";

/// Below this RMS (16-bit scale, about -50 dBFS) the clip is treated
/// as silence: Whisper invents text ("Thank you.") for empty audio.
const SILENCE_RMS: f64 = 100.0;

struct Recording {
	child: Child,
	path: PathBuf,
}

#[derive(Default)]
pub struct Voice {
	recording: Mutex<Option<Recording>>,
}

fn recorders(path: &str) -> [(&'static str, Vec<String>); 2] {
	[
		(
			"pw-record",
			vec![
				"--rate".into(),
				SAMPLE_RATE.into(),
				"--channels".into(),
				"1".into(),
				"--format".into(),
				"s16".into(),
				path.into(),
			],
		),
		(
			"arecord",
			vec![
				"-q".into(),
				"-f".into(),
				"S16_LE".into(),
				"-r".into(),
				SAMPLE_RATE.into(),
				"-c".into(),
				"1".into(),
				"-t".into(),
				"wav".into(),
				path.into(),
			],
		),
	]
}

/// Start recording from the default input. Errors if a recording is
/// already running.
#[tauri::command]
pub async fn voice_start(voice: tauri::State<'_, Voice>) -> Result<(), MoonError> {
	let mut slot = voice.recording.lock().await;
	if slot.is_some() {
		return Err(MoonError::invalid("already recording"));
	}
	let path = std::env::temp_dir().join(format!("moon-voice-{}.wav", std::process::id()));
	let _ = std::fs::remove_file(&path);
	let path_str = path.to_string_lossy().into_owned();
	for (program, args) in recorders(&path_str) {
		match Command::new(program).args(&args).kill_on_drop(true).spawn() {
			Ok(child) => {
				*slot = Some(Recording { child, path });
				return Ok(());
			}
			Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
			Err(err) => return Err(MoonError::internal(format!("could not start {program}: {err}"))),
		}
	}
	Err(MoonError::internal(
		"no recorder found: install PipeWire (pw-record) or alsa-utils (arecord)",
	))
}

/// Stop recording and return the clip as base64 WAV, or an empty
/// string when nothing above silence was captured.
#[tauri::command]
pub async fn voice_stop(voice: tauri::State<'_, Voice>) -> Result<String, MoonError> {
	let Some(mut recording) = voice.recording.lock().await.take() else {
		return Err(MoonError::invalid("not recording"));
	};
	finish(&mut recording.child).await;
	let bytes = std::fs::read(&recording.path);
	let _ = std::fs::remove_file(&recording.path);
	let bytes = bytes.map_err(|err| MoonError::internal(format!("could not read the recording: {err}")))?;
	if wav_rms(&bytes).is_none_or(|rms| rms < SILENCE_RMS) {
		return Ok(String::new());
	}
	Ok(base64::engine::general_purpose::STANDARD.encode(bytes))
}

/// Drop a running recording without returning it.
#[tauri::command]
pub async fn voice_cancel(voice: tauri::State<'_, Voice>) -> Result<(), MoonError> {
	if let Some(mut recording) = voice.recording.lock().await.take() {
		let _ = recording.child.kill().await;
		let _ = std::fs::remove_file(&recording.path);
	}
	Ok(())
}

/// SIGINT lets the recorder finalise the WAV header; kill it if it
/// doesn't exit promptly.
async fn finish(child: &mut Child) {
	#[cfg(unix)]
	if let Some(pid) = child.id().and_then(|pid| i32::try_from(pid).ok()) {
		// SAFETY: plain signal delivery to our own child.
		unsafe {
			libc::kill(pid, libc::SIGINT);
		}
	}
	if tokio::time::timeout(Duration::from_secs(3), child.wait())
		.await
		.is_err()
	{
		let _ = child.kill().await;
	}
}

/// RMS of a 16-bit PCM WAV's samples; `None` when it isn't one.
fn wav_rms(bytes: &[u8]) -> Option<f64> {
	if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
		return None;
	}
	let mut at = 12;
	while at + 8 <= bytes.len() {
		let id = &bytes[at..at + 4];
		let size = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().ok()?) as usize;
		let body = at + 8;
		if id == b"data" {
			// Recorders stopped mid-stream may leave the size unset.
			let end = body.saturating_add(size).min(bytes.len());
			let samples = bytes[body..end]
				.chunks_exact(2)
				.map(|pair| f64::from(i16::from_le_bytes([pair[0], pair[1]])));
			let (sum, count) = samples.fold((0.0, 0usize), |(sum, count), s| (sum + s * s, count + 1));
			if count == 0 {
				return Some(0.0);
			}
			return Some((sum / count as f64).sqrt());
		}
		at = body.saturating_add(size + size % 2);
	}
	None
}

#[cfg(test)]
mod tests {
	use super::*;

	fn wav(samples: &[i16]) -> Vec<u8> {
		let data: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
		let mut out = Vec::new();
		out.extend_from_slice(b"RIFF");
		out.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
		out.extend_from_slice(b"WAVEfmt ");
		out.extend_from_slice(&16u32.to_le_bytes());
		out.extend_from_slice(&[1, 0, 1, 0]);
		out.extend_from_slice(&16000u32.to_le_bytes());
		out.extend_from_slice(&32000u32.to_le_bytes());
		out.extend_from_slice(&[2, 0, 16, 0]);
		out.extend_from_slice(b"data");
		out.extend_from_slice(&(data.len() as u32).to_le_bytes());
		out.extend_from_slice(&data);
		out
	}

	#[test]
	fn rms_tells_speech_from_silence() {
		let quiet = wav(&[3, -2, 5, 0, -4]);
		let loud = wav(&[3000, -2500, 4000, -3500]);
		assert!(wav_rms(&quiet).unwrap() < SILENCE_RMS);
		assert!(wav_rms(&loud).unwrap() > SILENCE_RMS);
		assert_eq!(wav_rms(b"not a wav"), None);
	}
}
