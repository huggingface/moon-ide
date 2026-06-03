// App state for the companion PWA. Svelte 5 runes, single shared
// store (same convention as the desktop app's `state.svelte.ts`).
//
// The companion is read-mostly for now (13.4): it pairs, lists
// workspaces, and shows a workspace's coder status + session list.
// Sending prompts / committing land when the backend exposes those
// relay methods (see specs/roadmaps/phase-13-mobile-companion.md).

import { BridgeSocket, clearConnection, loadConnection, type Connection } from './transport';

// Wire shapes mirror the bridge's read-only method results, which in
// turn mirror moon-coder / moon-core types. Kept minimal — only the
// fields the UI renders.
export type WorkspaceSnapshot = {
	id: string;
	folders: Array<{ path: string; name: string }>;
	active_folder: string | null;
};

export type WorkspaceListing = {
	id: string;
	name: string;
	last_active_at: number | null;
	live: boolean;
};

export type CoderStatus = {
	signed_in: boolean;
	running_turn: boolean;
};

export type SessionSummary = {
	id: string;
	title: string;
	updated_at_ms: number;
};

type Phase = 'connecting' | 'pairing' | 'ready' | 'error';

class CompanionState {
	phase = $state<Phase>('connecting');
	error = $state<string | null>(null);

	connection = $state<Connection | null>(null);
	#socket: BridgeSocket | null = null;

	/** Host workspaces (the switcher). */
	workspaces = $state<WorkspaceListing[]>([]);
	loadingWorkspaces = $state(false);

	/** The workspace the user picked, or null while choosing. */
	activeWorkspace = $state<string | null>(null);

	coderStatus = $state<CoderStatus | null>(null);
	sessions = $state<SessionSummary[]>([]);
	loadingSessions = $state(false);

	/** Boot: if we already have a paired connection, reconnect; else pair. */
	async boot(): Promise<void> {
		const conn = loadConnection();
		if (!conn) {
			this.phase = 'pairing';
			return;
		}
		this.connection = conn;
		try {
			this.#socket = new BridgeSocket(conn.url);
			await this.#socket.open();
			this.phase = 'ready';
		} catch (e) {
			this.error = e instanceof Error ? e.message : String(e);
			this.phase = 'error';
		}
	}

	/** Pair using the QR/typed payload. `url` is `wss://host:port`. */
	async pair(url: string, code: string, label: string): Promise<void> {
		this.error = null;
		try {
			const socket = new BridgeSocket(url);
			await socket.open();
			const conn = await socket.pair(code, label);
			this.#socket = socket;
			this.connection = conn;
			this.phase = 'ready';
		} catch (e) {
			this.error = e instanceof Error ? e.message : String(e);
		}
	}

	/** Forget this device's pairing and return to the pair screen. */
	unpair(): void {
		clearConnection();
		this.#socket?.close();
		this.#socket = null;
		this.connection = null;
		this.activeWorkspace = null;
		this.coderStatus = null;
		this.sessions = [];
		this.phase = 'pairing';
	}

	async #call<T>(workspace: string, method: string, params: unknown = {}): Promise<T> {
		if (!this.#socket || !this.connection) {
			throw new Error('not connected');
		}
		return this.#socket.call<T>(this.connection.token, workspace, method, params);
	}

	/** Load the host's workspace list for the switcher. */
	async loadWorkspaces(): Promise<void> {
		if (!this.#socket || !this.connection) {
			return;
		}
		this.loadingWorkspaces = true;
		this.error = null;
		try {
			this.workspaces = await this.#socket.workspaces<WorkspaceListing[]>(this.connection.token);
		} catch (e) {
			this.error = e instanceof Error ? e.message : String(e);
		} finally {
			this.loadingWorkspaces = false;
		}
	}

	/** Open a workspace: load its coder status + session list. */
	async openWorkspace(workspace: string): Promise<void> {
		this.activeWorkspace = workspace;
		this.coderStatus = null;
		this.sessions = [];
		this.loadingSessions = true;
		try {
			this.coderStatus = await this.#call<CoderStatus>(workspace, 'coder_status');
			this.sessions = await this.#call<SessionSummary[]>(workspace, 'coder_list_sessions');
		} catch (e) {
			this.error = e instanceof Error ? e.message : String(e);
		} finally {
			this.loadingSessions = false;
		}
	}

	/** Back out of the active workspace to the switcher. */
	closeWorkspace(): void {
		this.activeWorkspace = null;
		this.coderStatus = null;
		this.sessions = [];
	}
}

export const app = new CompanionState();
