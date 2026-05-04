//! moon-container — Phase 2 workspace container plumbing.
//!
//! Two compose-project surfaces live here:
//!
//! - **Workspace shell** ([`Workspace`]) — the `dev` container the
//!   IDE uses for terminals, LSP, agents. One per workspace,
//!   project name `moon-ws-<id>`, IDE-managed.
//! - **Project services** ([`ProjectCompose`]) — each bound
//!   folder's own `docker-compose.yml`, run as a separate compose
//!   project (`moon-ws-<id>-<folder-slug>`). Started/stopped
//!   on demand by the user from the folder bar.
//!
//! The architectural backdrop is in
//! [`specs/containers.md`](../../../specs/containers.md); the
//! decision to use a host-shared Docker daemon (rather than
//! nesting Docker inside the workspace container) is
//! [ADR 0008](../../../specs/decisions/0008-host-shared-daemon.md).
//! The split between workspace shell and project services is the
//! [2026-04-29 amendment to ADR 0007](../../../specs/decisions/0007-compose-and-moon-base.md#amendment-2026-04-29--workspace-shell-vs-project-services).

pub mod compose;
pub mod discovery;
pub mod lifecycle;
pub mod project;
pub mod project_compose;

pub use compose::{
	generate_compose, BoundMount, ComposeRender, ComposeRenderOptions, SshAgentForward, SSH_AGENT_CONTAINER_PATH,
};
pub use discovery::{
	discover_compose_files, discover_compose_files_for_folders, discover_root_compose, ComposeDiscovery,
	DiscoveredCompose,
};
pub use lifecycle::{LifecycleError, Workspace, WorkspaceConfig, BOUND_FOLDERS_FILE, COMPOSE_FILE, DEFAULT_DEV_IMAGE};
pub use project::{folder_slug, project_name_for_folder, project_name_for_id, ProjectName, ProjectNameError};
pub use project_compose::{slug_for_folder_basename, ProjectCompose, ProjectComposeSnapshot};
