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
	/// containers, no `compose.yaml`, or both).
	Absent,
	/// One or more containers are `restarting` — actively
	/// transitioning. (A container that exists but has never
	/// started, i.e. compose's `created`, is **not** mapped to
	/// this — that means a `depends_on` precondition stalled,
	/// which is a `Failed` symptom.)
	Creating,
	/// At least one container is running and none are paused.
	/// Init containers that exited with code 0 alongside the
	/// running services don't disturb this.
	Running,
	/// At least one container is `paused`. Compose's pause is
	/// project-wide for our usage, so under normal moon-ide
	/// control this is all-or-nothing; mixed paused/running
	/// states only show up when a user paused individual
	/// containers outside the IDE.
	Paused,
	/// Every container is `exited` (zero exit code) or `created`
	/// without ever having started. moon-ide itself never drives
	/// into this — `Workspace::pause` uses pause, not stop — but
	/// we surface it if the user ran `docker compose stop` /
	/// `compose create` from outside.
	Stopped,
	/// One of: a container is `dead`, the daemon reported a
	/// state we don't recognise, a service exited with a
	/// non-zero code, or the project is in a mixed state where
	/// some services are running while others are stuck in
	/// `created` (stalled `depends_on`) or have exited. The UI
	/// shows the per-service detail so the user can decide.
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
	/// Process exit code. Compose emits `0` for non-exited
	/// states too, so this is meaningful only when
	/// `raw_state == "exited"`. The aggregation layer uses it
	/// to distinguish a successful init container (exit 0)
	/// from a failed long-running service (exit ≠ 0); the UI
	/// surfaces it next to the state for `exited` services.
	pub exit_code: i32,
	/// Healthcheck verdict, when one is declared (`healthy`,
	/// `unhealthy`, `starting`). Empty string when the service
	/// has no healthcheck.
	pub health: String,
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
