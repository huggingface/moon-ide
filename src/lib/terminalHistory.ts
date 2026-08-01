//! Shell-history capture for terminal panes.
//!
//! The persistence + restart features need "the command this
//! terminal last ran", and the only honest source is the shell's
//! own history. The backend prepends a one-line hook to
//! `PROMPT_COMMAND` in every spawned shell (see
//! `moon_terminal::HISTORY_ECHO_HOOK`), so just before each
//! prompt the shell itself echoes its newest history entry,
//! base64'd, with a `MOONCMD` marker we recognise in the output
//! stream. No screen-scraping, no synthetic keystrokes — the
//! shell's answer is authoritative (covers `cd`, aliases,
//! up-arrow-and-edit runs) and lands in the same output chunks
//! as everything else.
//!
//! Shells without `PROMPT_COMMAND` (zsh without a bash-compat
//! shim, fish, …) never emit the marker — the recorded command
//! then stays whatever the spawn/replay seeded, and everything
//! degrades to "restart replays the restore command".
//!
//! Pure module, no Svelte / no IPC. `TerminalTab.svelte` glues
//! it to the store's writer stream and to
//! `terminal.recordCommand`.

/** Marker prefix the shell hook prints before the base64
 * payload. Kept short and unlikely to collide with real output;
 * a collision only costs one stale history entry. */
export const HISTORY_MARKER = 'MOONCMD';

/** Matches CSI (`ESC [ … letter`) and OSC (`ESC ] … BEL` or
 * `ESC ] … ESC \`) sequences so the history-marker scan sees
 * the text the user sees. Matching ESC/BEL is the whole point
 * of this regex, so the `no-control-regex` lint is suppressed
 * on the line — there is no escape-free way to recognise an
 * ANSI escape. */
// oxlint-disable-next-line no-control-regex -- ANSI escape matching needs ESC/BEL.
const ANSI_ESCAPE_RE = /\u001b\[[0-9;]*[A-Za-z]|\u001b\][^\u0007\u001b]*(?:\u0007|\u001b\\)/g;

/** One output chunk's scan result: the text to feed xterm
 * (marker lines stripped) plus any command captured from a
 * marker line in this chunk. At most one capture per chunk in
 * practice (one prompt render = one hook echo); a burst of
 * several keeps the last, which is the newest history entry. */
export type HistoryScan = {
	visible: string;
	captured: string | null;
};

/** Scan one decoded output chunk for history markers. `buf`
 * carries any partial line left over from the previous chunk
 * (markers are line-oriented); return the new leftover. Marker
 * lines are stripped from the returned text so the user never
 * sees the hook's echo. */
export function scanHistoryChunk(buf: string, chunk: string): { buf: string; scan: HistoryScan } {
	const text = buf + chunk;
	const lines = text.split('\n');
	const nextBuf = lines.pop() ?? '';
	let visible = '';
	let captured: string | null = null;
	for (const line of lines) {
		const parsed = parseHistoryLine(line);
		if (parsed !== null) {
			captured = parsed;
			continue;
		}
		visible += `${line}\n`;
	}
	return { buf: nextBuf, scan: { visible, captured } };
}

/** Match one output line against the marker format:
 * `MOONCMD<base64>`, where the base64 decodes to the newest
 * history line. ANSI escapes are stripped first so a coloured
 * prompt can't split the marker across escape sequences.
 * Returns the decoded command, or `null` when the line isn't a
 * marker (or its payload doesn't decode). */
export function parseHistoryLine(line: string): string | null {
	const clean = line.replace(ANSI_ESCAPE_RE, '');
	const idx = clean.indexOf(HISTORY_MARKER);
	if (idx < 0) {
		return null;
	}
	const payload = clean.slice(idx + HISTORY_MARKER.length).trim();
	if (payload.length === 0) {
		return null;
	}
	try {
		const binary = atob(payload);
		const bytes = new Uint8Array(binary.length);
		for (let i = 0; i < binary.length; i++) {
			bytes[i] = binary.charCodeAt(i);
		}
		return new TextDecoder().decode(bytes);
	} catch {
		return null;
	}
}
