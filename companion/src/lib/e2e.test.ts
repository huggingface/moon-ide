import { hexToBytes, bytesToHex, utf8ToBytes } from '@noble/hashes/utils.js';
import { describe, expect, it } from 'vitest';

import {
	AD_CALL,
	AD_EVENT,
	AD_RESULT,
	Initiator,
	PAIR_PATTERN,
	PAIR_PROLOGUE,
	ReplayWindow,
	SESSION_PATTERN,
	inviteId,
	open,
	parsePairLink,
	seal,
} from './e2e';
// Generated from snow by `crates/moon-remote/src/e2e.rs` (`vectors`
// test) — the IDE side's reference transcript.
import v from './e2e-vectors.json';

const h = hexToBytes;

describe('Noise initiator matches the IDE (snow) byte for byte', () => {
	it('pairing handshake (IKpsk2)', () => {
		expect(v.pair_pattern).toBe(PAIR_PATTERN);
		expect(bytesToHex(PAIR_PROLOGUE)).toBe(v.pair_prologue);
		const init = new Initiator({
			pattern: PAIR_PATTERN,
			prologue: PAIR_PROLOGUE,
			localPrivate: h(v.phone_private),
			remotePublic: h(v.ide_public),
			psk: h(v.pair_psk),
			ephemeralPrivate: h(v.pair_phone_ephemeral),
		});
		expect(bytesToHex(init.writeMessage1(h(v.pair_payload)))).toBe(v.pair_msg1);
		expect(bytesToHex(init.readMessage2(h(v.pair_msg2)).payload)).toBe(v.pair_reply);
	});

	it('pairing reply from someone without the secret is rejected', () => {
		const init = new Initiator({
			pattern: PAIR_PATTERN,
			prologue: PAIR_PROLOGUE,
			localPrivate: h(v.phone_private),
			remotePublic: h(v.ide_public),
			psk: new Uint8Array(32).fill(9),
			ephemeralPrivate: h(v.pair_phone_ephemeral),
		});
		init.writeMessage1(h(v.pair_payload));
		expect(() => init.readMessage2(h(v.pair_msg2))).toThrow();
	});

	it('session handshake (IK) and split keys', () => {
		expect(v.session_pattern).toBe(SESSION_PATTERN);
		const init = new Initiator({
			pattern: SESSION_PATTERN,
			prologue: h(v.session_prologue),
			localPrivate: h(v.phone_private),
			remotePublic: h(v.ide_public),
			ephemeralPrivate: h(v.open_phone_ephemeral),
		});
		expect(bytesToHex(init.writeMessage1(new Uint8Array(0)))).toBe(v.open_msg1);
		const keys = init.readMessage2(h(v.open_msg2));
		expect(bytesToHex(keys.sendKey)).toBe(v.key_i2r);
		expect(bytesToHex(keys.recvKey)).toBe(v.key_r2i);
	});

	it('transport seal/open with explicit nonces and kind-tagged AD', () => {
		expect(bytesToHex(seal(h(v.key_i2r), v.call_n, AD_CALL, h(v.call_plain)))).toBe(v.call_sealed);
		const result = open(h(v.key_r2i), v.result_n, AD_RESULT, h(v.result_sealed));
		expect(result && bytesToHex(result)).toBe(v.result_plain);
		const event = open(h(v.key_r2i), v.event_n, AD_EVENT, h(v.event_sealed));
		expect(event && bytesToHex(event)).toBe(v.event_plain);
		// A result presented as an event must not authenticate.
		expect(open(h(v.key_r2i), v.result_n, AD_EVENT, h(v.result_sealed))).toBeNull();
	});

	it('invite id', () => {
		expect(inviteId(v.invite_secret)).toBe(v.invite_id);
	});
});

describe('ReplayWindow', () => {
	it('accepts reordering within the window and rejects replays', () => {
		const w = new ReplayWindow();
		for (const n of [5, 3, 4, 0, 2100, 100]) {
			expect(w.fresh(n)).toBe(true);
			w.mark(n);
			expect(w.fresh(n)).toBe(false);
		}
		expect(w.fresh(3)).toBe(false);
		expect(w.fresh(2099)).toBe(true);
	});
});

describe('parsePairLink', () => {
	it('parses a full link and a bare fragment', () => {
		const link = 'https://bridge.example/#pair=AB12-CD34&ide=my%20ide&ws=ws1&k=KEY&s=SECRET';
		expect(parsePairLink(link)).toEqual({
			bridgeUrl: 'wss://bridge.example',
			relayCode: 'AB12-CD34',
			ide: 'my ide',
			workspace: 'ws1',
			ideKey: 'KEY',
			secret: 'SECRET',
		});
		expect(parsePairLink('#pair=X&ws=w&k=K&s=S', 'https://h:53180')?.bridgeUrl).toBe('wss://h:53180');
	});

	it('rejects legacy code-only links', () => {
		expect(parsePairLink('https://bridge.example/#pair=AB12-CD34')).toBeNull();
		expect(parsePairLink(new TextDecoder().decode(utf8ToBytes('AB12-CD34')))).toBeNull();
	});
});
