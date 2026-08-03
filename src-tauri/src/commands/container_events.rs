//! Docker-events watcher for the workspace container.
//!
//! The container status pip (and everything downstream of it —
//! terminal restore, the auto-resume gate) is only truthful when
//! the IDE is the one driving container lifecycle. The moment the
//! daemon changes state on its own — a previous IDE session's
//! graceful `docker compose stop` landing seconds into a fresh
//! launch, a `docker stop` from an external terminal, the daemon
//! itself restarting — the pip keeps showing the last snapshot
//! until a focus event or a manual click re-polls. The visible
//! symptom is "pip green, then every container terminal dies at
//! once."
//!
//! This module closes that window: it tails `docker events`
//! filtered to the workspace's `dev` container and, on any
//! lifecycle action (start/stop/die/pause/…), re-snapshots the
//! status and re-broadcasts `container:state`. The frontend's
//! existing listener picks it up like any IDE-initiated change,
//! so the pip flips the moment the truth changes instead of on
//! the next poll.
//!
//! Design: ADR 0022 / phase-02's docker-events note, and
//! `container.svelte.ts`'s own "2.2 adds a watcher" comment.
//!
//! Best-effort by construction: if `docker events` can't be
//! spawned (no daemon, no CLI) the watcher exits after logging,
//! and the pip simply degrades to the old poll-on-focus cadence.

use moon_terminal::container_name_for_workspace;
use tauri::{AppHandle, Manager};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use crate::commands::container::emit_container_state;
use crate::state::AppState;

/// Docker event `Action` values that change the container's
/// high-level state. We ignore the noisy ones (exec_create /
/// exec_start fire on every terminal open, attach, top, …) —
/// only these move the state the pip reports.
const LIFECYCLE_ACTIONS: [&str; 7] = ["start", "stop", "die", "pause", "unpause", "kill", "destroy"];

/// Spawn the watcher. Fire-and-forget: the caller drops the
/// returned `JoinHandle`; the task lives until the process does
/// or the events stream ends. Called once from app setup after
/// the workspace id is known.
pub fn watch_container_events(app: AppHandle, workspace_id: String) {
	let container_name = container_name_for_workspace(&workspace_id);
	tauri::async_runtime::spawn(run(app, container_name));
}

async fn run(app: AppHandle, container_name: String) {
	// `docker events --format '{{json .}}'` streams one JSON
	// object per daemon event. We filter server-side to our
	// container so the stream stays quiet even on a busy
	// daemon.
	let child = Command::new("docker")
		.args([
			"events",
			"--format",
			"{{json .}}",
			"--filter",
			"type=container",
			"--filter",
			&format!("container={container_name}"),
		])
		.stdout(std::process::Stdio::piped())
		.stderr(std::process::Stdio::null())
		.spawn();
	let mut child = match child {
		Ok(c) => c,
		Err(err) => {
			tracing::warn!(error = %err, "container events watcher: failed to spawn `docker events`");
			return;
		}
	};
	let Some(stdout) = child.stdout.take() else {
		tracing::warn!("container events watcher: no stdout on `docker events` child");
		return;
	};
	tracing::info!(container = %container_name, "container events watcher: started");

	let mut lines = BufReader::new(stdout).lines();
	loop {
		match lines.next_line().await {
			Ok(Some(line)) => {
				if is_lifecycle_event(&line) {
					on_lifecycle_event(&app).await;
				}
			}
			// Stream ended (daemon closed the connection, docker
			// CLI exited) — stop quietly. A restart loop would
			// need backoff policy; the next IDE launch / lifecycle
			// command re-establishes coverage, and focus-polling
			// still works in the gap.
			Ok(None) => break,
			Err(err) => {
				tracing::warn!(error = %err, "container events watcher: read error");
				break;
			}
		}
	}
	let _ = child.kill().await;
	tracing::info!(container = %container_name, "container events watcher: stopped");
}

/// Cheap string check before any JSON parse: does this event
/// carry one of the lifecycle actions? The `--format {{json .}}`
/// payload has `"Action":"die"` shape; a substring match on
/// `"Action":"<verb>"` is enough and avoids a serde dependency
/// for a one-field read.
fn is_lifecycle_event(line: &str) -> bool {
	LIFECYCLE_ACTIONS
		.iter()
		.any(|action| line.contains(&format!("\"Action\":\"{action}\"")))
}

/// A lifecycle event fired: re-snapshot the truth and broadcast.
/// We don't trust the event's own payload for the new state —
/// `die`/`stop` race each other and the final `compose ps`
/// reading is the only honest answer. Goes through the ADR 0020
/// cache, which at this point has aged past its 1 s TTL for any
/// reading taken before the stop landed.
async fn on_lifecycle_event(app: &AppHandle) {
	let state = app.state::<AppState>();
	let container = match crate::commands::container::workspace_handle(&state).await {
		Ok(c) => c,
		Err(err) => {
			tracing::warn!(error = %err, "container events watcher: failed to build workspace handle");
			return;
		}
	};
	match container.status().await {
		Ok(status) => emit_container_state(app, &status),
		Err(err) => tracing::warn!(error = %err, "container events watcher: status query failed"),
	}
}
