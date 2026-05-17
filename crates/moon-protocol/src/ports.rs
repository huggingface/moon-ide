//! Workspace port-forwarding shapes shared between backend and frontend.
//!
//! See [`specs/containers.md`](../../../specs/containers.md) §
//! "Network and port forwarding". The user declares forwards
//! per-workspace; the backend serves them via a single shared
//! `alpine/socat` proxy sidecar (`moon-ws-<id>-ports-1`) on the
//! workspace's default network, so adding/removing a forward
//! never recreates the dev container — terminals and any
//! in-flight `bun dev` survive the change.
//!
//! Per AGENTS.md "no premature migrations": these structs change
//! freely until the roadmap is done; on-disk shapes in
//! `session.json` rely on `#[serde(default)]` for back-compat.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// One declared forward from the host to the dev container.
///
/// `host_port` defaults to `container_port` on the picker; the
/// user only edits it when two workspaces want the same dev-side
/// port and need to disambiguate on the host (workspace A:
/// `3000 -> 3000`, workspace B: `3001 -> 3000`).
///
/// Loopback-only: the proxy sidecar binds `127.0.0.1:host_port`
/// on the host. We deliberately don't expose a `0.0.0.0` toggle
/// yet — per AGENTS.md "hardcode first, configure later", that
/// lands when somebody actually wants to reach a workspace dev
/// server from another device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ForwardedPort {
	/// Port the user's process listens on **inside** the dev
	/// container. The proxy sidecar opens
	/// `tcp:moon-ws-<id>-dev-1:<container_port>` on each accepted
	/// host-side connection.
	pub container_port: u16,
	/// Port the proxy sidecar publishes on the host
	/// (`127.0.0.1:host_port`). Defaults to `container_port` on
	/// the picker; the user re-types it on conflict.
	pub host_port: u16,
	/// Free-form human label (`"vite"`, `"api"`, …). Surfaced in
	/// the Ports panel; doesn't affect the wire shape and may be
	/// empty for forwards the user didn't bother naming.
	#[serde(default)]
	pub label: String,
}

/// Per-port live state, as reported by `ports_status`.
///
/// The panel renders one of these per declared forward; the dot
/// next to each row is keyed off the variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
pub enum ForwardedPortHealth {
	/// Proxy sidecar is up, host port is bound, everything looks
	/// healthy. Connecting from the host should reach the dev
	/// process.
	Live,
	/// Host port is busy on the host before the sidecar got to
	/// bind it (something else on the machine is already using
	/// `127.0.0.1:host_port`). Surfaced via a pre-flight probe
	/// so the user sees the real error instead of a generic
	/// `docker run` failure.
	HostPortBusy,
	/// Proxy sidecar isn't running — most likely because the
	/// workspace shell itself is down (we tear the sidecar down
	/// when the dev compose project goes away). The forward is
	/// still persisted on disk and will come back up when the
	/// shell is restarted.
	ProxyDown,
}

/// One row of `ports_status` output: the user-facing forward
/// definition plus its current health.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ForwardedPortStatus {
	pub forward: ForwardedPort,
	pub health: ForwardedPortHealth,
}

/// Result of `ports_set`. Returned synchronously so the picker
/// can show "applied" vs "host port busy on these N entries"
/// without a round-trip through the event bus.
///
/// On success (`conflicts` empty) the forward set on disk and
/// the running sidecar both reflect `applied`. On a partial
/// conflict (one or more of the requested host ports were busy
/// before the sidecar could bind them) we still persist the
/// requested set — the user's intent is recorded for the next
/// retry — but flag the conflicting entries here so the picker
/// can render them with a red dot and a "host port busy" tooltip.
/// The non-conflicting entries do come up in the same sidecar
/// run.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct PortsApplyResult {
	pub applied: Vec<ForwardedPort>,
	pub conflicts: Vec<ForwardedPort>,
}
