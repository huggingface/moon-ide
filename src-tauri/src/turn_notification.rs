//! Desktop notification + sound when a user-facing coder turn
//! settles while the window is unfocused (ADR 0089). Driven by
//! [`crate::agent_indicator::AgentIndicator`], which owns the
//! focus state.

use moon_coder::{TurnOutcome, TurnSettled};
use tauri::AppHandle;

/// Notification body cap, in chars. Notification servers clamp
/// the visible body to a few lines anyway; this keeps a long
/// final report from shipping kilobytes over D-Bus.
const BODY_MAX_CHARS: usize = 280;

/// Freedesktop sound-theme file, played directly instead of via
/// the notification `sound-name` hint: not every notification
/// server (COSMIC, Cinnamon) honours the hint.
#[cfg(target_os = "linux")]
const SOUND_FILE: &str = "/usr/share/sounds/freedesktop/stereo/complete.oga";

pub fn notify(app: &AppHandle, workspace_id: &str, event: &TurnSettled) {
	play_sound();
	let (summary, body) = match &event.outcome {
		TurnOutcome::Complete { last_assistant } => (format!("{workspace_id}: {}", event.title), last_assistant.as_str()),
		TurnOutcome::Error { message } => (format!("{workspace_id}: {} (failed)", event.title), message.as_str()),
	};
	let body = escape_markup(&truncate(body, BODY_MAX_CHARS));
	let mut notification = notify_rust::Notification::new();
	notification
		.appname("moon-ide")
		.icon("moon-ide")
		.summary(&summary)
		.body(&body);
	#[cfg(all(unix, not(target_os = "macos")))]
	{
		notification
			.hint(notify_rust::Hint::DesktopEntry("moon-ide".into()))
			.action("default", "Focus window");
	}
	let app = app.clone();
	// `show` is a blocking D-Bus round trip, and waiting for the
	// click blocks until the notification is dismissed.
	tauri::async_runtime::spawn_blocking(move || {
		let handle = match notification.show() {
			Ok(handle) => handle,
			Err(err) => {
				tracing::warn!(error = %err, "failed to show turn notification");
				return;
			}
		};
		#[cfg(all(unix, not(target_os = "macos")))]
		handle.wait_for_action(|action| {
			if action == "default" {
				crate::focus_socket::focus_main_window(&app);
			}
		});
		#[cfg(not(all(unix, not(target_os = "macos"))))]
		{
			let _ = (handle, app);
		}
	});
}

#[cfg(target_os = "linux")]
fn play_sound() {
	tauri::async_runtime::spawn_blocking(|| {
		// PipeWire first (current distros), PulseAudio fallback.
		for player in ["pw-play", "paplay"] {
			let status = std::process::Command::new(player)
				.arg(SOUND_FILE)
				.stdin(std::process::Stdio::null())
				.stdout(std::process::Stdio::null())
				.stderr(std::process::Stdio::null())
				.status();
			if matches!(status, Ok(s) if s.success()) {
				return;
			}
		}
		tracing::debug!("no sound player succeeded for turn notification");
	});
}

#[cfg(not(target_os = "linux"))]
fn play_sound() {}

/// Collapse whitespace runs (markdown paragraphs, code fences) so
/// the few visible lines carry text, then cap at `max` chars.
fn truncate(text: &str, max: usize) -> String {
	let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
	if collapsed.chars().count() <= max {
		return collapsed;
	}
	let mut out: String = collapsed.chars().take(max - 1).collect();
	out.push('…');
	out
}

/// Servers advertising `body-markup` parse a subset of HTML; an
/// unescaped `<` in agent prose would swallow or reject the body.
fn escape_markup(text: &str) -> String {
	text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn truncate_collapses_and_caps() {
		assert_eq!(truncate("a\n\n  b", 10), "a b");
		assert_eq!(truncate("abcdefghij", 5), "abcd…");
		assert_eq!(truncate("ééééé", 5), "ééééé");
	}

	#[test]
	fn escapes_markup() {
		assert_eq!(escape_markup("a < b && c > d"), "a &lt; b &amp;&amp; c &gt; d");
	}
}
