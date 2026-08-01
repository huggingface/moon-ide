import { describe, expect, it } from 'vitest';

import { HISTORY_MARKER, parseHistoryLine, scanHistoryChunk } from './terminalHistory';

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

describe('parseHistoryLine', () => {
	it('decodes a plain marker line', () => {
		expect(parseHistoryLine(`${HISTORY_MARKER}${b64('pnpm dev')}`)).toBe('pnpm dev');
	});

	it('returns null for a non-marker line', () => {
		expect(parseHistoryLine('total 48')).toBeNull();
	});

	it('returns null for an empty payload', () => {
		expect(parseHistoryLine(HISTORY_MARKER)).toBeNull();
	});

	it('strips a coloured prompt before matching', () => {
		// ESC[32m … ESC[0m around the marker, as a green prompt
		// would render it. ESC built with fromCharCode so the
		// test source holds no literal control chars.
		const esc = String.fromCharCode(27);
		const line = `${esc}[32m$user${esc}[0m ${HISTORY_MARKER}${b64('git status')}`;
		expect(parseHistoryLine(line)).toBe('git status');
	});

	it('round-trips commands with spaces, quotes, and unicode', () => {
		const cmd = `NODE_ENV=dev pnpm --filter "@hf/app" run "build:éà"`;
		expect(parseHistoryLine(`${HISTORY_MARKER}${b64(cmd)}`)).toBe(cmd);
	});

	it('returns null for garbage base64', () => {
		expect(parseHistoryLine(`${HISTORY_MARKER}!!!not-base64!!!`)).toBeNull();
	});

	it('anchors on the first marker occurrence', () => {
		// A stray second marker word inside the line becomes part
		// of the first payload and just fails the decode — one
		// stale entry, not a crash.
		expect(parseHistoryLine(`${HISTORY_MARKER}${b64('a')} ${HISTORY_MARKER}${b64('b')}`)).toBeNull();
	});
});

describe('scanHistoryChunk', () => {
	it('passes through text without markers', () => {
		const { buf, scan } = scanHistoryChunk('', 'hello\nworld\n');
		expect(scan.visible).toBe('hello\nworld\n');
		expect(scan.captured).toBeNull();
		expect(buf).toBe('');
	});

	it('strips a marker line from the visible text', () => {
		const { scan } = scanHistoryChunk('', `before\n${HISTORY_MARKER}${b64('ls -la')}\nafter\n`);
		expect(scan.visible).toBe('before\nafter\n');
		expect(scan.captured).toBe('ls -la');
	});

	it('keeps the last marker when several arrive in one chunk', () => {
		const { scan } = scanHistoryChunk('', `${HISTORY_MARKER}${b64('one')}\n${HISTORY_MARKER}${b64('two')}\n`);
		expect(scan.captured).toBe('two');
	});

	it('carries a partial line across chunks', () => {
		const first = scanHistoryChunk('', `prompt$ ${HISTORY_MARKER.slice(0, 4)}`);
		expect(first.scan.visible).toBe('');
		expect(first.buf).toBe(`prompt$ ${HISTORY_MARKER.slice(0, 4)}`);
		const second = scanHistoryChunk(first.buf, `${HISTORY_MARKER.slice(4)}${b64('cmd')}\n`);
		expect(second.scan.captured).toBe('cmd');
	});

	it('does not emit a trailing partial line as visible', () => {
		const { buf, scan } = scanHistoryChunk('', 'output without newline');
		expect(scan.visible).toBe('');
		expect(buf).toBe('output without newline');
	});
});
