import { describe, expect, it } from 'vitest';

import { HISTORY_MARKER, scanHistoryChunk } from './terminalHistory';

/** base64-encode the way the shell hook does (UTF-8 bytes →
 * single-line base64). */
function b64(text: string): string {
	const bytes = new TextEncoder().encode(text);
	let binary = '';
	for (const b of bytes) {
		binary += String.fromCharCode(b);
	}
	return btoa(binary);
}

/** The hook's token: marker + base64 payload + the CRLF the
 * echo emits before the next prompt. */
function token(cmd: string): string {
	return `${HISTORY_MARKER}${b64(cmd)}\r\n`;
}

describe('scanHistoryChunk', () => {
	it('passes through output without markers', () => {
		const { buf, scan } = scanHistoryChunk('', 'hello world\r\n');
		expect(scan.visible).toBe('hello world\r\n');
		expect(scan.captured).toBeNull();
		expect(buf).toBe('');
	});

	it('strips a marker token and captures its command', () => {
		const { scan } = scanHistoryChunk('', `before${token('ls -la')}after`);
		expect(scan.visible).toBe('beforeafter');
		expect(scan.captured).toBe('ls -la');
	});

	it('consumes the token’s trailing newline so no blank line remains', () => {
		// The prompt follows the hook echo directly; stripping the
		// token must not leave the `\r\n` behind as an empty row.
		const { scan } = scanHistoryChunk('', `${token('git status')}$ `);
		expect(scan.visible).toBe('$ ');
		expect(scan.captured).toBe('git status');
	});

	it('keeps the last marker when several arrive in one chunk', () => {
		const { scan } = scanHistoryChunk('', `${token('one')}mid${token('two')}`);
		expect(scan.captured).toBe('two');
		expect(scan.visible).toBe('mid');
	});

	it('round-trips commands with spaces, quotes, and unicode', () => {
		const cmd = `NODE_ENV=dev pnpm --filter "@hf/app" run "build:éà"`;
		const { scan } = scanHistoryChunk('', token(cmd));
		expect(scan.captured).toBe(cmd);
	});

	it('rejoins a token split across two chunks', () => {
		const full = token('pnpm dev');
		const cut = full.indexOf('CMD') + 2; // split mid-marker
		const first = scanHistoryChunk('', full.slice(0, cut));
		expect(first.scan.visible).toBe('');
		expect(first.buf.length).toBeGreaterThan(0);
		const second = scanHistoryChunk(first.buf, full.slice(cut));
		expect(second.scan.captured).toBe('pnpm dev');
		expect(second.scan.visible).toBe('');
	});

	it('flushes a held run once the next chunk proves it is not a marker', () => {
		// Chunk ends in "MOON" — a possible marker prefix, so it's
		// held. The next chunk shows it was just the word "MOON".
		const first = scanHistoryChunk('', 'the MOON');
		expect(first.scan.visible).toBe('the ');
		expect(first.buf).toBe('MOON');
		const second = scanHistoryChunk(first.buf, ' is out\r\n');
		expect(second.scan.visible).toBe('MOON is out\r\n');
		expect(second.scan.captured).toBeNull();
	});

	it('does not hold a long base64-ish run that is not marker-shaped', () => {
		// A build log ending in base64 must flush, not stall.
		const tail = 'YWJjZGVmZ2hpamtsbW5vcA==';
		const { buf, scan } = scanHistoryChunk('', `log ${tail}`);
		expect(scan.visible).toBe(`log ${tail}`);
		expect(buf).toBe('');
	});

	it('leaves a marker-shaped but undecodable token intact', () => {
		// `MOONCMD` followed by non-base64 fails the payload match,
		// so the replace never fires and the text survives.
		const { scan } = scanHistoryChunk('', `${HISTORY_MARKER}!!! not ours\r\n`);
		expect(scan.visible).toBe(`${HISTORY_MARKER}!!! not ours\r\n`);
		expect(scan.captured).toBeNull();
	});
});
