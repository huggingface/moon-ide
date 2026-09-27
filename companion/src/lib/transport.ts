// WSS transport to moon-bridge. This is the companion's equivalent of
// the desktop app's `invoke` — every workspace call goes through here.
//
// Wire shapes mirror `crates/moon-bridge/src/serve.rs`. Two layers:
//
// - Relay routing: `pair` → device token, `workspaces`, and `call` /
//   `subscribe` frames addressed by (ide, workspace). The relay sees
//   only these envelopes.
// - End-to-end (ADR 0087): every workspace method rides inside an
//   `e2e_call` sealed for the IDE, and event streams are sealed per
//   session. The relay can route, delay or drop — not read or forge.
//
// Calls carry a `call_id` the bridge echoes (replies arrive in
// completion order); pair / workspaces fall back to a FIFO queue.

import {
	E2eError,
	asRecord,
	E2eSession,
	UNKNOWN_SESSION,
	cacheSession,
	cachedSession,
	dropSession,
	ideFor,
	pairWithIde,
	sessionKey,
	type IdePin,
	type PairLink,
} from './e2e';

const STORAGE_KEY = 'moon-bridge-connection';

/** Relay-routed methods that may travel in the clear (ADR 0087): the
 * handshakes themselves, plus `workspace_launch`, which the relay
 * routes to any live process of the IDE and so can't ride a
 * per-process session. Starting an existing workspace is all it
 * grants. */
const PLAINTEXT_METHODS = new Set(['workspace_launch']);

export const NOT_PAIRED_WITH_IDE =
	"This phone isn't paired with that IDE. Open its Companion panel (or run `moon-remote pair`) and scan the pairing QR.";

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
	/** Sealed event streams by session id. Events that don't decrypt
	 * under one of these are dropped — a relay can't inject rows. */
	#streamSessions = new Map<string, E2eSession>();
	/** Streams to re-open when their session is replaced (the IDE
	 * process restarted and forgot the old one). */
	#streams = new Map<string, { token: string; workspace: string; ide: string }>();
	#nextRequestId = 1;
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
					this.#routeEvent(msg.event);
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
			// Client-side deadline: a zombie-OPEN socket (phone
			// radio slept, TCP dead underneath) accepts the send
			// and then nothing ever comes back — without this the
			// waiter parks forever and the UI looks hung until the
			// OS finally notices the dead connection. The bridge's
			// own forward timeout is shorter, so a live socket
			// always answers (result or error) well within this.
			const deadline = setTimeout(() => {
				if (this.#pendingCalls.delete(callId)) {
					reject(new BridgeError('call timed out'));
				}
			}, 30_000);
			this.#pendingCalls.set(callId, {
				resolve: (m) => {
					clearTimeout(deadline);
					resolve(m);
				},
				reject: (e) => {
					clearTimeout(deadline);
					reject(e);
				},
			});
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

	#routeEvent(raw: unknown): void {
		const frame = asRecord(raw);
		const sid = frame.sid;
		const session = typeof sid === 'string' ? this.#streamSessions.get(sid) : undefined;
		if (!session) {
			return;
		}
		let plain: unknown;
		try {
			plain = session.openEvent(frame);
		} catch {
			return;
		}
		if (plain && typeof plain === 'object') {
			// Attribution comes from the session, not from the
			// relay-injected `ide`/`workspace` tags on the wrapper.
			const envelope = asRecord(plain);
			envelope.ide = session.ide;
			envelope.workspace = session.workspace;
		}
		this.#onEvent?.(plain);
	}

	/** One-shot plaintext relay call — only for handshakes and
	 * `PLAINTEXT_METHODS`. */
	async #plainCall(token: string, workspace: string, method: string, params: unknown, ide: string): Promise<unknown> {
		const reply = await this.#sendCall({ type: 'call', token, workspace, method, params, ide });
		if (reply.type === 'error') {
			throw new BridgeError(reply.message);
		}
		if (reply.type !== 'result') {
			throw new BridgeError('unexpected reply to call');
		}
		return reply.value;
	}

	#pinFor(ide: string): IdePin {
		const pin = ideFor(this.url, ide);
		if (!pin) {
			throw new BridgeError(NOT_PAIRED_WITH_IDE);
		}
		return pin;
	}

	/** The live session for (ide, workspace), handshaking if needed.
	 * Concurrent callers share one handshake. */
	#session(token: string, workspace: string, ide: string): Promise<E2eSession> {
		const key = sessionKey(this.url, ide, workspace);
		const existing = cachedSession(key);
		if (existing) {
			return existing;
		}
		const pin = this.#pinFor(ide);
		const opening = E2eSession.open((m, p) => this.#plainCall(token, workspace, m, p, ide), pin, ide, workspace);
		cacheSession(key, opening);
		return opening;
	}

	/** Forget a session the IDE no longer knows, handshake again, and
	 * move any event streams onto the new one. */
	async #renew(token: string, workspace: string, ide: string, stale: Promise<E2eSession>): Promise<E2eSession> {
		dropSession(sessionKey(this.url, ide, workspace), stale);
		const fresh = await this.#session(token, workspace, ide);
		const streamKey = sessionKey(this.url, ide, workspace);
		if (this.#streams.has(streamKey)) {
			this.#sendSubscribe(token, workspace, ide, fresh);
		}
		return fresh;
	}

	#sendSubscribe(token: string, workspace: string, ide: string, session: E2eSession): void {
		const ws = this.#ws;
		if (!ws || ws.readyState !== WebSocket.OPEN) {
			return;
		}
		this.#streamSessions.set(session.sid, session);
		ws.send(JSON.stringify({ type: 'subscribe', token, workspace, ide, params: { sid: session.sid } }));
	}

	/** Pin an IDE from a pairing link (ADR 0087). Needs a relay token
	 * already (`pair` first when the phone is new to this relay). */
	async pairIde(token: string, link: PairLink, label: string): Promise<IdePin> {
		return pairWithIde((m, p) => this.#plainCall(token, link.workspace, m, p, link.ide), link, label);
	}

	/** Subscribe to a workspace's coder event stream. Events arrive via
	 * the `onEvent` handler, decrypted; this send has no direct reply.
	 * `ide` selects the carrier (empty = local, present = remote IDE). */
	subscribe(token: string, workspace: string, ide = ''): void {
		this.#streams.set(sessionKey(this.url, ide, workspace), { token, workspace, ide });
		this.#session(token, workspace, ide).then(
			(session) => this.#sendSubscribe(token, workspace, ide, session),
			() => {
				// Surfaced by the next call on this workspace, which
				// fails the same way with a message the UI shows.
			},
		);
	}

	/** Invoke a relayed method on `workspace`, end-to-end sealed for
	 * the IDE (ADR 0087). `ide` selects the carrier (empty = local,
	 * present = remote IDE). */
	async call<T = unknown>(
		token: string,
		workspace: string,
		method: string,
		params: unknown = {},
		ide = '',
	): Promise<T> {
		if (PLAINTEXT_METHODS.has(method)) {
			// Untyped JSON boundary — the caller declares the shape it expects.
			// eslint-disable-next-line typescript-eslint/no-unsafe-type-assertion
			return (await this.#plainCall(token, workspace, method, params, ide)) as T;
		}
		let pending = this.#session(token, workspace, ide);
		for (let attempt = 0; ; attempt++) {
			const session = await pending;
			const id = this.#nextRequestId++;
			let reply: unknown;
			try {
				reply = await this.#plainCall(token, workspace, 'e2e_call', session.sealCall(id, method, params), ide);
			} catch (e) {
				if (attempt === 0 && e instanceof BridgeError && e.message.includes(UNKNOWN_SESSION)) {
					pending = this.#renew(token, workspace, ide, pending);
					continue;
				}
				throw e;
			}
			try {
				// Untyped JSON boundary — the caller declares the shape it expects.
				// eslint-disable-next-line typescript-eslint/no-unsafe-type-assertion
				return session.openResult(reply, id) as T;
			} catch (e) {
				throw e instanceof E2eError ? new BridgeError(e.message) : e;
			}
		}
	}
}
