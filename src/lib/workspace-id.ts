// Active workspace id for this process. Process-per-workspace
// (Phase 7): each `moon-ide` process is pinned to one workspace
// at startup, picked from `--workspace <slug>` (or auto-derived
// in preboot mode). The frontend asks the backend exactly once
// via `app_info` and caches the answer for the rest of the
// window's lifetime.
//
// Preboot mode (no workspace bound, empty catalog) returns
// `null` — call sites that need a workspace must guard against
// it. The regular IDE chrome doesn't render in preboot mode;
// only the WorkspaceCreate landing does.

import { ipc } from './ipc';

import type { AppInfo, WorkspaceId } from './protocol';

let cached: AppInfo | null = null;

/**
 * Returns the cached workspace id, or `null` if the process is
 * in preboot mode (no workspace bound). Throws if the caller
 * forgot to await `resolveAppInfo` on boot — that's a
 * programming error, not a runtime condition.
 */
export function currentWorkspaceId(): WorkspaceId | null {
	if (cached === null) {
		throw new Error('currentWorkspaceId() called before resolveAppInfo()');
	}
	return cached.workspaceId;
}

/**
 * Returns the cached app info. Same lifetime contract as
 * `currentWorkspaceId`.
 */
export function currentAppInfo(): AppInfo {
	if (cached === null) {
		throw new Error('currentAppInfo() called before resolveAppInfo()');
	}
	return cached;
}

/**
 * Resolve and cache the boot info. Idempotent: repeated calls
 * return the same cached value. Awaited from `App.svelte`'s
 * `hydrate` before any workspace-scoped IPC.
 */
export async function resolveAppInfo(): Promise<AppInfo> {
	if (cached !== null) {
		return cached;
	}
	cached = await ipc.appInfo();
	return cached;
}
