//! Container lifecycle shapes shared between backend and frontend.
//!
//! See [`specs/containers.md`](../../../specs/containers.md). The
//! Phase 2.0 surface is small: snapshot the compose project's
//! state, list its services, and ferry a state-changed event up
//! to the status pip. Per-service start/stop/restart and log
//! tailing land in 2.1+ and grow this module accordingly.
//!
//! Per AGENTS.md "no premature migrations": these structs change
//! freely until the roadmap is done — there are no aliases or
//! version-tolerant readers.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// High-level state of the workspace's compose project.
///
/// Drives the status-pip glyph and the "Set up" / "Pause" /
/// "Resume" affordances. Callers should treat any unknown
/// variant as `Failed` rather than `Running` — see
/// `crates/moon-container/src/lifecycle.rs#aggregate_state`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "lowercase")]
pub enum ContainerState {
	/// No compose project exists for this workspace (no
	/// containers, no `.moon/compose.yaml`, or both).
	Absent,
	/// `up -d` is in flight, or one or more containers are
	/// `created` / `restarting`.
	Creating,
	/// At least one container is running and none are paused.
	Running,
	/// At least one container is `paused`. Compose's pause is
	/// project-wide for our usage, so under normal moon-ide
	/// control this is all-or-nothing; mixed paused/running
	/// states only show up when a user paused individual
	/// containers outside the IDE.
	Paused,
	/// Containers exist but every one is `exited`. moon-ide
	/// itself never drives into this — `Workspace::pause` uses
	/// pause, not stop — but we surface it if the user ran
	/// `docker compose stop` from outside.
	Stopped,
	/// A container is `dead`, the daemon reported a state we
	/// don't recognise, or compose itself errored. The UI shows
	/// the per-service detail so the user can decide.
	Failed,
}

/// One container in the compose project, as reported by
/// `docker compose ps --format json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ServiceStatus {
	/// Compose service name (`dev`, `mongo`, `redis`, …).
	pub name: String,
	/// Raw Docker container state (`running`, `paused`,
	/// `exited`, `created`, `restarting`, `dead`). Forwarded
	/// verbatim so the UI can show it without us re-encoding
	/// nuance away.
	pub raw_state: String,
}

/// Snapshot returned by `container_status` and embedded in
/// every [`ContainerStateChange`] event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ContainerStatus {
	pub state: ContainerState,
	pub services: Vec<ServiceStatus>,
}

/// Payload of the `container:state` Tauri event, broadcast after
/// every lifecycle command (and, eventually, from the docker
/// events watcher 2.2 will add).
///
/// `workspace_id` is included so the frontend can route the
/// event to the right workspace once multi-window arrives — for
/// 2.0 it always matches the active workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ContainerStateChange {
	pub workspace_id: String,
	pub status: ContainerStatus,
}
