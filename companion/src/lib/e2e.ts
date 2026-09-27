// Companion end-to-end encryption, phone side (ADR 0087). Mirrors
// `crates/moon-remote/src/e2e.rs`: the IDE is always the Noise
// responder, the phone the initiator. Byte-compatibility with snow is
// pinned by `e2e-vectors.json` (generated from the Rust side).
//
// - Pairing: `Noise_IKpsk2` against the IDE key from the pairing link,
//   with the link's one-time secret as PSK. The IDE pins our key.
// - Sessions: `Noise_IK` per (IDE, workspace); the workspace slug is in
//   the prologue.
// - Transport: the split keys used directly with explicit nonces and a
//   replay window, because replies and events come back out of order.

import { chacha20poly1305 } from '@noble/ciphers/chacha.js';
import { x25519 } from '@noble/curves/ed25519.js';
import { hmac } from '@noble/hashes/hmac.js';
import { sha256 } from '@noble/hashes/sha2.js';
import { concatBytes, randomBytes, utf8ToBytes } from '@noble/hashes/utils.js';

export const PAIR_PATTERN = 'Noise_IKpsk2_25519_ChaChaPoly_SHA256';
export const SESSION_PATTERN = 'Noise_IK_25519_ChaChaPoly_SHA256';
export const PAIR_PROLOGUE = utf8ToBytes('moon-companion-pair-v1');
const SESSION_PROLOGUE_PREFIX = utf8ToBytes('moon-companion-session-v1\u0000');
export const AD_CALL = utf8ToBytes('moon-e2e-v1:c');
export const AD_RESULT = utf8ToBytes('moon-e2e-v1:r');
export const AD_EVENT = utf8ToBytes('moon-e2e-v1:e');
/** The IDE lost the session (restart, eviction, sibling process):
 * handshake again. */
export const UNKNOWN_SESSION = 'e2e: unknown session';

const REPLAY_WINDOW = 2048;
const STORE_KEY = 'moon-e2e-v1';

export class E2eError extends Error {}

/** Index an untyped JSON value; non-objects read as empty. */
export function asRecord(value: unknown): Record<string, unknown> {
	if (value === null || typeof value !== 'object') {
		return {};
	}
	// Narrowed to a non-null object just above; field reads stay `unknown`.
	// eslint-disable-next-line typescript-eslint/no-unsafe-type-assertion
	return value as Record<string, unknown>;
}

export function b64urlEncode(bytes: Uint8Array): string {
	let bin = '';
	for (const b of bytes) {
		bin += String.fromCharCode(b);
	}
	return btoa(bin).replaceAll('+', '-').replaceAll('/', '_').replace(/=+$/, '');
}

export function b64urlDecode(s: string): Uint8Array {
	const std = s.replaceAll('-', '+').replaceAll('_', '/');
	const bin = atob(std + '='.repeat((4 - (std.length % 4)) % 4));
	const out = new Uint8Array(bin.length);
	for (let i = 0; i < bin.length; i++) {
		out[i] = bin.charCodeAt(i);
	}
	return out;
}

/** Noise ChaChaPoly nonce: 32 zero bits then the 64-bit counter, LE. */
function nonce(n: number): Uint8Array {
	const out = new Uint8Array(12);
	new DataView(out.buffer).setBigUint64(4, BigInt(n), true);
	return out;
}

export function seal(key: Uint8Array, n: number, ad: Uint8Array, plaintext: Uint8Array): Uint8Array {
	return chacha20poly1305(key, nonce(n), ad).encrypt(plaintext);
}

export function open(key: Uint8Array, n: number, ad: Uint8Array, ciphertext: Uint8Array): Uint8Array | null {
	try {
		return chacha20poly1305(key, nonce(n), ad).decrypt(ciphertext);
	} catch {
		return null;
	}
}

class SymmetricState {
	ck: Uint8Array;
	h: Uint8Array;
	k: Uint8Array | null = null;
	n = 0;

	constructor(protocolName: string) {
		const name = utf8ToBytes(protocolName);
		if (name.length <= 32) {
			this.h = new Uint8Array(32);
			this.h.set(name);
		} else {
			this.h = sha256(name);
		}
		this.ck = this.h;
	}

	mixHash(data: Uint8Array): void {
		this.h = sha256(concatBytes(this.h, data));
	}

	/** Noise HKDF: the first two outputs, plus the third on demand. */
	#hkdf(ikm: Uint8Array): { o1: Uint8Array; o2: Uint8Array; o3: () => Uint8Array } {
		const temp = hmac(sha256, this.ck, ikm);
		const o1 = hmac(sha256, temp, Uint8Array.of(1));
		const o2 = hmac(sha256, temp, concatBytes(o1, Uint8Array.of(2)));
		return { o1, o2, o3: () => hmac(sha256, temp, concatBytes(o2, Uint8Array.of(3))) };
	}

	mixKey(ikm: Uint8Array): void {
		const { o1, o2 } = this.#hkdf(ikm);
		this.ck = o1;
		this.k = o2;
		this.n = 0;
	}

	mixKeyAndHash(ikm: Uint8Array): void {
		const { o1, o2, o3 } = this.#hkdf(ikm);
		this.ck = o1;
		this.mixHash(o2);
		this.k = o3();
		this.n = 0;
	}

	encryptAndHash(plaintext: Uint8Array): Uint8Array {
		const out = this.k ? seal(this.k, this.n++, this.h, plaintext) : plaintext;
		this.mixHash(out);
		return out;
	}

	decryptAndHash(ciphertext: Uint8Array): Uint8Array {
		let out = ciphertext;
		if (this.k) {
			const plain = open(this.k, this.n++, this.h, ciphertext);
			if (!plain) {
				throw new E2eError('handshake failed: the IDE could not prove it holds its key');
			}
			out = plain;
		}
		this.mixHash(ciphertext);
		return out;
	}

	split(): [Uint8Array, Uint8Array] {
		const { o1, o2 } = this.#hkdf(new Uint8Array(0));
		return [o1, o2];
	}
}

export type HandshakeResult = { payload: Uint8Array; sendKey: Uint8Array; recvKey: Uint8Array };

/** Initiator of `IK` / `IKpsk2` (the only patterns the phone runs). */
export class Initiator {
	#ss: SymmetricState;
	#s: Uint8Array;
	#e: Uint8Array;
	#rs: Uint8Array;
	#psk: Uint8Array | null;

	constructor(opts: {
		pattern: string;
		prologue: Uint8Array;
		localPrivate: Uint8Array;
		remotePublic: Uint8Array;
		psk?: Uint8Array;
		/** Tests only: pin the ephemeral to reproduce snow's vectors. */
		ephemeralPrivate?: Uint8Array;
	}) {
		this.#ss = new SymmetricState(opts.pattern);
		this.#s = opts.localPrivate;
		this.#rs = opts.remotePublic;
		this.#psk = opts.psk ?? null;
		this.#e = opts.ephemeralPrivate ?? randomBytes(32);
		this.#ss.mixHash(opts.prologue);
		this.#ss.mixHash(this.#rs);
	}

	/** `-> e, es, s, ss` + payload. */
	writeMessage1(payload: Uint8Array): Uint8Array {
		const ss = this.#ss;
		const ePub = x25519.getPublicKey(this.#e);
		ss.mixHash(ePub);
		if (this.#psk) {
			ss.mixKey(ePub);
		}
		ss.mixKey(x25519.getSharedSecret(this.#e, this.#rs));
		const sealedStatic = ss.encryptAndHash(x25519.getPublicKey(this.#s));
		ss.mixKey(x25519.getSharedSecret(this.#s, this.#rs));
		return concatBytes(ePub, sealedStatic, ss.encryptAndHash(payload));
	}

	/** `<- e, ee, se [, psk]` + payload, then split. */
	readMessage2(message: Uint8Array): HandshakeResult {
		if (message.length < 32 + 16) {
			throw new E2eError('handshake failed: truncated reply');
		}
		const ss = this.#ss;
		const re = message.slice(0, 32);
		ss.mixHash(re);
		if (this.#psk) {
			ss.mixKey(re);
		}
		ss.mixKey(x25519.getSharedSecret(this.#e, re));
		ss.mixKey(x25519.getSharedSecret(this.#s, re));
		if (this.#psk) {
			ss.mixKeyAndHash(this.#psk);
		}
		const payload = ss.decryptAndHash(message.slice(32));
		const [sendKey, recvKey] = ss.split();
		return { payload, sendKey, recvKey };
	}
}

/** Sliding-window replay filter over explicit nonces (same algorithm
 * as the IDE side). */
export class ReplayWindow {
	#top = 0;
	/** One byte per slot — simpler than bit twiddling at 2 kB. */
	#seen = new Uint8Array(REPLAY_WINDOW);

	fresh(n: number): boolean {
		if (n >= this.#top) {
			return true;
		}
		if (this.#top - n > REPLAY_WINDOW) {
			return false;
		}
		return this.#seen[n % REPLAY_WINDOW] !== 1;
	}

	mark(n: number): void {
		if (n >= this.#top) {
			if (n + 1 - this.#top >= REPLAY_WINDOW) {
				this.#seen.fill(0);
			} else {
				for (let k = this.#top; k <= n; k++) {
					this.#seen[k % REPLAY_WINDOW] = 0;
				}
			}
			this.#top = n + 1;
		}
		this.#seen[n % REPLAY_WINDOW] = 1;
	}
}

// ---- Persistent pins ----

export type IdePin = {
	/** The IDE host's X25519 public key, base64url. */
	key: string;
	label: string;
	deviceId: string;
	pairedAt: number;
};

type Store = {
	device: { priv: string; pub: string } | null;
	/** Keyed by `pinKey(bridgeUrl, ide)`. */
	ides: Record<string, IdePin>;
};

function loadStore(): Store {
	try {
		const raw = localStorage.getItem(STORE_KEY);
		if (raw) {
			// localStorage holds exactly what saveStore wrote.
			// eslint-disable-next-line typescript-eslint/no-unsafe-type-assertion
			return JSON.parse(raw) as Store;
		}
	} catch {
		// Fall through to a fresh store.
	}
	return { device: null, ides: {} };
}

function saveStore(store: Store): void {
	localStorage.setItem(STORE_KEY, JSON.stringify(store));
}

export function pinKey(bridgeUrl: string, ide: string): string {
	return `${bridgeUrl}|${ide}`;
}

/** This phone's static keypair, created on first use. */
export function deviceKeys(): { priv: Uint8Array; pub: Uint8Array } {
	const store = loadStore();
	if (!store.device) {
		const priv = randomBytes(32);
		store.device = { priv: b64urlEncode(priv), pub: b64urlEncode(x25519.getPublicKey(priv)) };
		saveStore(store);
	}
	return { priv: b64urlDecode(store.device.priv), pub: b64urlDecode(store.device.pub) };
}

export function ideFor(bridgeUrl: string, ide: string): IdePin | null {
	return loadStore().ides[pinKey(bridgeUrl, ide)] ?? null;
}

function savePin(bridgeUrl: string, ide: string, pin: IdePin): void {
	const store = loadStore();
	store.ides[pinKey(bridgeUrl, ide)] = pin;
	saveStore(store);
}

/** Drop every pin and the device key (the user unpairs this phone). */
export function forgetAll(): void {
	localStorage.removeItem(STORE_KEY);
	sessions.clear();
}

// ---- Pairing links ----

export type PairLink = {
	/** `wss://…` derived from the link's origin. */
	bridgeUrl: string;
	relayCode: string;
	ide: string;
	workspace: string;
	ideKey: string;
	secret: string;
};

/** Parse a pairing link (`https://host/#pair=…&ide=…&ws=…&k=…&s=…`) or
 * its bare fragment. `origin` resolves a bare fragment. */
export function parsePairLink(text: string, origin = ''): PairLink | null {
	const trimmed = text.trim();
	const hashAt = trimmed.indexOf('#');
	if (hashAt < 0) {
		return null;
	}
	const base = hashAt === 0 ? origin : trimmed.slice(0, hashAt);
	const host = /^https?:\/\/([^/?#]+)/.exec(base)?.[1];
	const params = new URLSearchParams(trimmed.slice(hashAt + 1));
	const relayCode = params.get('pair');
	const ideKey = params.get('k');
	const secret = params.get('s');
	const workspace = params.get('ws');
	if (!host || !relayCode || !ideKey || !secret || !workspace) {
		return null;
	}
	return {
		bridgeUrl: `wss://${host}`,
		relayCode,
		ide: params.get('ide') ?? '',
		workspace,
		ideKey,
		secret,
	};
}

export function inviteId(secret: string): string {
	return b64urlEncode(sha256(utf8ToBytes(secret)).slice(0, 12));
}

type PlainCall = (method: string, params: unknown) => Promise<unknown>;

function field(obj: unknown, name: string): string {
	const value = asRecord(obj)[name];
	if (typeof value !== 'string') {
		throw new E2eError(`malformed e2e reply: missing \`${name}\``);
	}
	return value;
}

/** Run `e2e_pair` over a plaintext relay call and pin the IDE. Fails
 * if whoever answered doesn't hold both the IDE key and the link's
 * secret — i.e. a relay trying to pair itself in the middle. */
export async function pairWithIde(call: PlainCall, link: PairLink, label: string): Promise<IdePin> {
	const device = deviceKeys();
	const init = new Initiator({
		pattern: PAIR_PATTERN,
		prologue: PAIR_PROLOGUE,
		localPrivate: device.priv,
		remotePublic: b64urlDecode(link.ideKey),
		psk: b64urlDecode(link.secret),
	});
	const msg1 = init.writeMessage1(utf8ToBytes(JSON.stringify({ label })));
	const reply = await call('e2e_pair', { invite: inviteId(link.secret), msg: b64urlEncode(msg1) });
	const { payload } = init.readMessage2(b64urlDecode(field(reply, 'msg')));
	const info: unknown = JSON.parse(new TextDecoder().decode(payload));
	const pin: IdePin = {
		key: link.ideKey,
		label: (() => {
			try {
				return field(info, 'ide_label');
			} catch {
				return link.ide || 'IDE';
			}
		})(),
		deviceId: field(info, 'device_id'),
		pairedAt: Date.now(),
	};
	savePin(link.bridgeUrl, link.ide, pin);
	return pin;
}

export type SealedFrame = { sid: string; n: number; c: string };

/** An encrypted channel to one (IDE, workspace). */
export class E2eSession {
	readonly sid: string;
	readonly ide: string;
	readonly workspace: string;
	#send: Uint8Array;
	#recv: Uint8Array;
	#n = 0;
	#window = new ReplayWindow();

	constructor(sid: string, ide: string, workspace: string, keys: { sendKey: Uint8Array; recvKey: Uint8Array }) {
		this.sid = sid;
		this.ide = ide;
		this.workspace = workspace;
		this.#send = keys.sendKey;
		this.#recv = keys.recvKey;
	}

	static async open(call: PlainCall, pin: IdePin, ide: string, workspace: string): Promise<E2eSession> {
		const device = deviceKeys();
		const init = new Initiator({
			pattern: SESSION_PATTERN,
			prologue: concatBytes(SESSION_PROLOGUE_PREFIX, utf8ToBytes(workspace)),
			localPrivate: device.priv,
			remotePublic: b64urlDecode(pin.key),
		});
		const reply = await call('e2e_open', { msg: b64urlEncode(init.writeMessage1(new Uint8Array(0))) });
		const keys = init.readMessage2(b64urlDecode(field(reply, 'msg')));
		return new E2eSession(field(reply, 'sid'), ide, workspace, keys);
	}

	sealCall(id: number, method: string, params: unknown): SealedFrame {
		const n = this.#n++;
		const plain = utf8ToBytes(JSON.stringify({ id, method, params }));
		return { sid: this.sid, n, c: b64urlEncode(seal(this.#send, n, AD_CALL, plain)) };
	}

	#openIn(frame: unknown, ad: Uint8Array): unknown {
		const record = asRecord(frame);
		const n = record.n;
		const c = record.c;
		if (typeof n !== 'number' || typeof c !== 'string' || !this.#window.fresh(n)) {
			throw new E2eError('e2e: replayed or malformed frame');
		}
		const plain = open(this.#recv, n, ad, b64urlDecode(c));
		if (!plain) {
			throw new E2eError('e2e: frame failed authentication');
		}
		this.#window.mark(n);
		return JSON.parse(new TextDecoder().decode(plain));
	}

	/** Decrypt an `e2e_call` reply; `id` must echo the request's. */
	openResult(frame: unknown, id: number): unknown {
		const reply = this.#openIn(frame, AD_RESULT);
		const record = asRecord(reply);
		if (record.id !== id) {
			throw new E2eError('e2e: reply belongs to a different request');
		}
		if (typeof record.err === 'string') {
			throw new E2eError(record.err);
		}
		return record.ok;
	}

	openEvent(frame: unknown): unknown {
		return this.#openIn(frame, AD_EVENT);
	}
}

/** Live sessions, keyed by bridge + IDE + workspace. They outlive a
 * socket: the IDE holds session state per process, not per relay
 * connection, so a reconnect keeps its keys and counters. */
const sessions = new Map<string, Promise<E2eSession>>();

export function sessionKey(bridgeUrl: string, ide: string, workspace: string): string {
	return `${bridgeUrl}|${ide}|${workspace}`;
}

export function cachedSession(key: string): Promise<E2eSession> | undefined {
	return sessions.get(key);
}

export function cacheSession(key: string, session: Promise<E2eSession>): void {
	sessions.set(key, session);
	// A failed handshake mustn't poison the cache.
	session.catch(() => {
		if (sessions.get(key) === session) {
			sessions.delete(key);
		}
	});
}

export function dropSession(key: string, session: Promise<E2eSession>): void {
	if (sessions.get(key) === session) {
		sessions.delete(key);
	}
}
