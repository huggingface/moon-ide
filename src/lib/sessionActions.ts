// Session gestures shared by the sidebar's sessions list and the coder
// panel's session bar.
import { writeText as clipboardWriteText } from '@tauri-apps/plugin-clipboard-manager';
import { openUrl } from '@tauri-apps/plugin-opener';
import { coder } from './coder.svelte';
import { ipc } from './ipc';
import { formatError, type CoderSessionSummary } from './protocol';
import { workspace } from './state.svelte';

// A session is associated with a **branch** (ADR 0028): the branch
// its work was committed onto, or its worktree's branch if it
// hasn't committed yet. Prefer the committed one — it tracks the
// live branch after a commit-on-new-branch.
export function sessionBranch(session: CoderSessionSummary): string | null {
	return session.committed_branch ?? session.worktree_branch ?? null;
}

// A coordinator (orchestrator) session — ADR 0030. The backend
// stamps `mode: "coordinator"` on the header. `null` (no summary
// yet) is not one.
export function isCoordinatorSession(session: CoderSessionSummary | null): boolean {
	return session?.mode === 'coordinator';
}

// The bound worktree folder currently checked out on `branch`, if
// any. Resolved live (not from which header field set it), so it's
// correct after a branch is moved into / out of a worktree.
export function worktreeFolderForBranch(branch: string | null): string | null {
	if (branch === null) {
		return null;
	}
	return (
		workspace.workspace?.folders.find((f) => f.origin.kind === 'worktree' && f.origin.branch === branch)?.path ?? null
	);
}

// Click on a session's branch chip: go to wherever the branch
// lives. If it's checked out in a worktree, focus that worktree
// folder (the file tree / SCM follow; the per-project session list
// stays put). Otherwise `git switch` the project's main checkout to
// it (git refuses + flashes on a dirty tree) — never a worktree,
// whose session would silently find another branch's code.
export async function goToSessionBranch(session: CoderSessionSummary): Promise<void> {
	const branch = sessionBranch(session);
	if (branch === null) {
		return;
	}
	const worktreePath = worktreeFolderForBranch(branch);
	if (worktreePath !== null) {
		await workspace.setActiveFolder(worktreePath);
		return;
	}
	const origin = workspace.activeFolder?.origin;
	if (origin?.kind === 'worktree') {
		await workspace.setActiveFolder(origin.parentPath);
	}
	await workspace.switchToBranch({ kind: 'local', name: branch });
}

/** Open a session's raw JSONL trace in the editor as a host-direct
 *  file (same machinery as Ctrl+O for files outside the workspace).
 *  Works for parent sessions and sub-agent ids alike — both live
 *  under the active folder's slug on disk. The trace lives on the
 *  *host* `XDG_DATA_HOME` even when the project is running in a
 *  container, which is exactly what the host-direct file path
 *  delivers.
 *
 *  Empty / never-persisted sessions surface as a flash via the
 *  backend's `not found` error; the user can keep working. */
export async function openSessionTrace(id: string): Promise<void> {
	let path: string;
	try {
		path = await ipc.coder.sessionJsonlPath(id);
	} catch (err) {
		workspace.flash(`Could not open trace: ${formatError(err)}`);
		return;
	}
	await workspace.openHostFile(path);
}

/** Open an uploaded trace's Hub URL in the host's default browser,
 *  or copy it to the clipboard when `copy` is set (Alt-click). Only
 *  offered for sessions with an `uploaded[id]` marker on the bucket
 *  binding, so a URL should always resolve; a failure still flashes. */
export async function openSessionTraceOnHub(sessionId: string, copy: boolean): Promise<void> {
	let url: string;
	try {
		url = await coder.hubSessionUrl(sessionId);
	} catch (err) {
		workspace.flash(`Could not resolve Hub URL: ${formatError(err)}`);
		return;
	}
	if (copy) {
		// Prefer the Tauri clipboard plugin: `navigator.clipboard`
		// is unreliable on WebKitGTK (silently no-ops when the
		// triggering element isn't a focused input), and Alt-click
		// fires on a button that doesn't take focus. Fall back to
		// `navigator.clipboard` only if the plugin call throws,
		// and surface a flash on hard failure rather than
		// swallowing — matches the `Copy path` pattern in
		// EditorTabs / FileTree.
		try {
			await clipboardWriteText(url);
			workspace.flash('Trace URL copied to clipboard.');
		} catch {
			try {
				await navigator.clipboard.writeText(url);
				workspace.flash('Trace URL copied to clipboard.');
			} catch {
				workspace.flash('Could not copy trace URL.');
			}
		}
		return;
	}
	void openUrl(url);
}

/** Show the coder panel with focus in its composer, so a session
 *  picked or started from anywhere is one keystroke from a prompt. */
export function revealComposer(): void {
	coder.setPanelVisible(true);
	coder.composerFocusTick += 1;
}

export async function startNewSession(): Promise<void> {
	await coder.newSession();
	revealComposer();
}

export async function startNewCoordinatorSession(): Promise<void> {
	await coder.newCoordinatorSession();
	revealComposer();
}
