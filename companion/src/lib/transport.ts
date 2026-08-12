// WSS transport to moon-bridge. This is the companion's equivalent of
// the desktop app's `invoke` — every workspace call goes through here.
//
// Wire shapes mirror `crates/moon-bridge/src/serve.rs`:
//   out: { type: "pair", code, label } | { type: "call", token, workspace, method, params }
//   in:  { type: "paired", device_id, token } | { type: "result", value } | { type: "error", message }
//
// The connection is paired once (device token persisted in
// localStorage), then reused for calls. Each call is matched to its
// reply by send-order: the bridge answers one frame per message on a
// single connection, and the UI issues calls sequentially, so a FIFO
// queue of pending resolvers is sufficient and avoids needing request
// ids on the wire.

const STORAGE_KEY = 'moon-bridge-connection';

export type Connection = {
	url: string;
	token: string;
	deviceId: string;
};

type ServerMessage =
	| { type: 'paired'; device_id: string; token: string }
	| { type: 'workspaces'; workspaces: unknown }
	| { type: 'result'; value: unknown; call_id?: number }
	| { type: 'event'; event: unknown }
	| { type: 'error'; message: string; call_id?: number };

export class BridgeError extends Error {}

/** Load the persisted connection (set after a successful pair). */
export function loadConnection(): Connection | null {
	const raw = localStorage.getItem(STORAGE_KEY);
	if (!raw) {
		return null;
	}
	try {
		// localStorage holds exactly what saveConnection wrote.
		// eslint-disable-next-line typescript-eslint/no-unsafe-type-assertion
		return JSON.parse(raw) as Connection;
	} catch {
		return null;
	}
}

function saveConnection(conn: Connection): void {
	localStorage.setItem(STORAGE_KEY, JSON.stringify(conn));
}

/** Forget the paired connection (the user "unpairs" on this device). */
export function clearConnection(): void {
	localStorage.removeItem(STORAGE_KEY);
}

/**
 * A live socket to the bridge. Construct with a `wss://…` URL, call
 * `open()`, then either `pair()` (first time) or `call()` (already
 * holding a device token).
 */
export class BridgeSocket {
	#ws: WebSocket | null = null;
	/** FIFO reply queue for id-less requests (pair / workspaces —
	 * the bridge answers those inline, in order). */
	#pending: Array<{ resolve: (m: ServerMessage) => void; reject: (e: Error) => void }> = [];
	/** Id-keyed waiters for `call` requests. Forwarded calls run
	 * concurrently on the IDE and reply in *completion* order —
	 * FIFO matching mis-delivered every reply the moment two calls
	 * overlapped (the "SCM card missing after refresh" class of
	 * bug). The bridge echoes `call_id`; matching by it makes reply
	 * order irrelevant. */
	#pendingCalls = new Map<number, { resolve: (m: ServerMessage) => void; reject: (e: Error) => void }>();
	#nextCallId = 1;
	#onEvent: ((event: unknown) => void) | null = null;
	readonly url: string;

	constructor(url: string) {
		this.url = url;
	}

	/** Register a handler for server-pushed `event` frames (the coder
	 * stream). Pushed events bypass the request/reply FIFO. */
	onEvent(handler: (event: unknown) => void): void {
		this.#onEvent = handler;
	}

	open(): Promise<void> {
		return new Promise((resolve, reject) => {
			const ws = new WebSocket(this.url);
			this.#ws = ws;
			ws.addEventListener('open', () => resolve());
			ws.addEventListener('error', () => reject(new BridgeError(`could not connect to ${this.url}`)));
			ws.addEventListener('close', () => {
				const err = new BridgeError('connection closed');
				for (const p of this.#pending) {
					p.reject(err);
				}
				this.#pending = [];
				for (const p of this.#pendingCalls.values()) {
					p.reject(err);
				}
				this.#pendingCalls.clear();
			});
			ws.addEventListener('message', (ev) => {
				let msg: ServerMessage;
				try {
					const data = typeof ev.data === 'string' ? ev.data : '';
					// The bridge only ever sends our ServerMessage shapes.
					// eslint-disable-next-line typescript-eslint/no-unsafe-type-assertion
					msg = JSON.parse(data) as ServerMessage;
				} catch {
					const waiter = this.#pending.shift();
					waiter?.reject(new BridgeError('malformed reply from bridge'));
					return;
				}
				// Pushed events are unsolicited — route them to the event
				// handler rather than consuming a pending reply.
				if (msg.type === 'event') {
					this.#onEvent?.(msg.event);
					return;
				}
				// Correlated replies (calls) resolve by id; everything
				// else falls back to the FIFO queue. An id we no longer
				// track (reply raced the close-reject) is dropped rather
				// than mis-fed to a FIFO waiter.
				if ((msg.type === 'result' || msg.type === 'error') && typeof msg.call_id === 'number') {
					const waiter = this.#pendingCalls.get(msg.call_id);
					this.#pendingCalls.delete(msg.call_id);
					waiter?.resolve(msg);
					return;
				}
				this.#pending.shift()?.resolve(msg);
			});
		});
	}

	close(): void {
		this.#ws?.close();
		this.#ws = null;
	}

	/** Whether the underlying WebSocket is currently open. A
	 * backgrounded PWA's socket drops silently; the app checks this
	 * on resume to decide whether to reconnect. */
	isOpen(): boolean {
		return this.#ws?.readyState === WebSocket.OPEN;
	}

	#send(payload: unknown): Promise<ServerMessage> {
		const ws = this.#ws;
		if (!ws || ws.readyState !== WebSocket.OPEN) {
			return Promise.reject(new BridgeError('not connected'));
		}
		return new Promise((resolve, reject) => {
			this.#pending.push({ resolve, reject });
			ws.send(JSON.stringify(payload));
		});
	}

	/** `#send` for `call` requests: attaches a correlation id and
	 * parks the waiter in the id-keyed map. Old bridges that don't
	 * echo `call_id` never resolve these — their replies land in
	 * the FIFO path, which has no waiter, so the caller hits the
	 * bridge's own timeout error instead of a mismatched payload. */
	#sendCall(payload: Record<string, unknown>): Promise<ServerMessage> {
		const ws = this.#ws;
		if (!ws || ws.readyState !== WebSocket.OPEN) {
			return Promise.reject(new BridgeError('not connected'));
		}
		const callId = this.#nextCallId++;
		return new Promise((resolve, reject) => {
			this.#pendingCalls.set(callId, { resolve, reject });
			ws.send(JSON.stringify({ ...payload, call_id: callId }));
		});
	}

	/** Present a pairing code; on success persists + returns the connection. */
	async pair(code: string, label: string): Promise<Connection> {
		const reply = await this.#send({ type: 'pair', code, label });
		if (reply.type === 'error') {
			throw new BridgeError(reply.message);
		}
		if (reply.type !== 'paired') {
			throw new BridgeError('unexpected reply to pair');
		}
		const conn: Connection = { url: this.url, token: reply.token, deviceId: reply.device_id };
		saveConnection(conn);
		return conn;
	}

	/** List the host's workspaces (the switcher), authenticated by `token`. */
	async workspaces<T = unknown>(token: string): Promise<T> {
		const reply = await this.#send({ type: 'workspaces', token });
		if (reply.type === 'error') {
			throw new BridgeError(reply.message);
		}
		if (reply.type !== 'workspaces') {
			throw new BridgeError('unexpected reply to workspaces');
		}
		// Untyped JSON boundary — the caller declares the shape it expects.
		// eslint-disable-next-line typescript-eslint/no-unsafe-type-assertion
		return reply.workspaces as T;
	}

	/** Subscribe to a workspace's coder event stream. Events arrive via
	 * the `onEvent` handler; this send has no direct reply. `ide`
	 * selects the carrier (empty = local, present = remote IDE). */
	subscribe(token: string, workspace: string, ide = ''): void {
		const ws = this.#ws;
		if (!ws || ws.readyState !== WebSocket.OPEN) {
			return;
		}
		ws.send(JSON.stringify({ type: 'subscribe', token, workspace, ide }));
	}

	/** Invoke a relayed method on `workspace`, authenticated by `token`.
	 * `ide` selects the carrier (empty = local, present = remote IDE). */
	async call<T = unknown>(
		token: string,
		workspace: string,
		method: string,
		params: unknown = {},
		ide = '',
	): Promise<T> {
		const reply = await this.#sendCall({ type: 'call', token, workspace, method, params, ide });
		if (reply.type === 'error') {
			throw new BridgeError(reply.message);
		}
		if (reply.type !== 'result') {
			throw new BridgeError('unexpected reply to call');
		}
		// Untyped JSON boundary — the caller declares the shape it expects.
		// eslint-disable-next-line typescript-eslint/no-unsafe-type-assertion
		return reply.value as T;
	}
}
