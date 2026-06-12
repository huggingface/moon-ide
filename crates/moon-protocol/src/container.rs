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
	/// non-zero code, a running service is attached to no
	/// network (see [`ServiceStatus::networkless`]), or the
	/// project is in a mixed state where some services are
	/// running while others are stuck in `created` (stalled
	/// `depends_on`) or have exited. The UI shows the
	/// per-service detail so the user can decide.
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
	/// True when the container is up but attached to **no**
	/// network — the residue of a failed start (typically a
	/// host-port conflict) whose rollback wiped the container's
	/// endpoint config. Such a service is unreachable by name
	/// and publishes nothing, no matter how healthy it claims to
	/// be, and only a recreate fixes it; the backend
	/// auto-recreates on the next lifecycle action and the UI
	/// surfaces it as failed meanwhile. Always `false` for
	/// containers that aren't running (stopped containers hold
	/// no endpoints, so the question is meaningless).
	pub networkless: bool,
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
/// events watcher 2.2 will add). Phase 7 made each window one
/// process bound to one workspace, so the event implicitly
/// scopes to that process — no `workspace_id` field needed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ContainerStateChange {
	pub status: ContainerStatus,
}

/// Status of a single bound folder's compose project (its own
/// `docker-compose.yml`).
///
/// Distinct from [`ContainerStatus`]: a folder may not have a
/// compose file at all (most folders the user opens just to edit
/// code). When that's the case, `compose_file` and
/// `project_name` are `None` and the UI hides the folder-bar
/// indicator entirely. When they're `Some`, the inner `status`
/// follows the same `Absent` / `Running` / `Failed` / etc.
/// vocabulary as the workspace shell.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ProjectComposeStatus {
	/// Absolute path to the bound folder this snapshot refers to.
	pub folder_path: String,
	/// Absolute path to the user-owned compose file driving the
	/// folder's services, or `None` if the folder has none.
	pub compose_file: Option<String>,
	/// Compose project name on the daemon
	/// (`moon-ws-<id>-<folder-slug>`). `None` when
	/// `compose_file` is `None`.
	pub project_name: Option<String>,
	/// Aggregated state of the folder's compose project.
	/// `Absent` covers both "no compose file" and "compose file
	/// present, never brought up". The UI distinguishes the two
	/// via `compose_file.is_some()`.
	pub status: ContainerStatus,
}

/// Payload of the `project_compose:state` Tauri event,
/// broadcast after every per-folder lifecycle mutation. Keyed
/// on `folder_path` so the UI can update one folder bar without
/// re-querying the others.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ProjectComposeStateChange {
	pub folder_path: String,
	pub project: ProjectComposeStatus,
}

/// Single line of `docker compose logs` output streamed to the
/// frontend. Emitted on the `compose_logs:line` Tauri event;
/// the frontend buffers per `stream_id` so multiple log tabs
/// don't interleave.
///
/// `channel` is `"stdout"` or `"stderr"` so the renderer can
/// colour them differently (compose itself only writes to
/// stdout for service output, but errors from the docker CLI
/// arrive on stderr — keeping them separated lets the UI
/// surface those distinctly).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct LogStreamLine {
	pub stream_id: String,
	pub channel: String,
	pub text: String,
}

/// Final event for a log stream, fired exactly once when the
/// underlying `docker compose logs -f` child exits. `code` is
/// the process's exit code if we caught it, or `None` if the
/// supervisor was cancelled before it could observe the wait()
/// result. Either way the frontend should mark the tab as no
/// longer streaming and stop sending close calls for this id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct LogStreamClosed {
	pub stream_id: String,
	pub code: Option<i32>,
}
