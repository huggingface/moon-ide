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

/** One full marker token in the stream: `MOONCMD<base64>`,
 * optionally followed by the `\r\n` the hook's own echo
 * produces. The payload is captured for decoding; the trailing
 * newline is consumed so stripping the marker doesn't leave a
 * blank line where the hook echoed. The hook writes a bare
 * token with no ANSI framing of its own; any surrounding prompt
 * escapes are the shell's and left intact. */
// oxlint-disable-next-line no-control-regex -- the trailing \r\n match needs control chars.
const MARKER_TOKEN_RE = /MOONCMD([A-Za-z0-9+/]+={0,2})\r?\n?/g;

/** A trailing run of marker-eligible characters at the very end
 * of a chunk. A `MOONCMD<base64>` token has no internal
 * delimiter, so if a chunk ends mid-token the partial run must
 * be held back until the next chunk completes it. Matched run
 * is a candidate; `scanHistoryChunk` decides whether it's
 * genuinely a marker prefix worth holding or just ordinary
 * output that happens to end in base64-ish characters. */
const TRAILING_RUN_RE = /[A-Za-z0-9+/]+=?$/;

/** One output chunk's scan result: the text to feed xterm
 * (marker tokens stripped) plus the command captured from the
 * last marker token in this chunk, if any. */
export type HistoryScan = {
	visible: string;
	captured: string | null;
};

/** Scan one decoded output chunk for history markers. `buf`
 * carries any trailing marker-eligible run held back from the
 * previous chunk (a token split across the boundary); returns
 * the new leftover. Marker tokens are stripped from the
 * returned text so the user never sees the hook's echo, and the
 * decoded command from the last token is handed back for the
 * store to record. */
export function scanHistoryChunk(buf: string, chunk: string): { buf: string; scan: HistoryScan } {
	const text = buf + chunk;
	// Work out how much of the trailing run to hold back for the
	// next chunk. A run is only worth holding if it's short
	// (could still be the `MOONCMD` prefix, or the marker plus a
	// payload that's still growing); a long run is ordinary
	// output (a build log line ending in base64) and flushing it
	// avoids stalling the stream behind a false positive.
	const run = TRAILING_RUN_RE.exec(text);
	const tail = run === null ? '' : run[0];
	const holdback = holdbackLength(tail);
	const body = holdback === 0 ? text : text.slice(0, -holdback);

	let captured: string | null = null;
	const visible = body.replace(MARKER_TOKEN_RE, (whole: string, payload: string) => {
		const decoded = decodePayload(payload);
		if (decoded === null) {
			// Marker-shaped but not our hook's payload (a stray
			// `MOONCMD…` in real output). Leave it untouched
			// rather than eating legitimate text.
			return whole;
		}
		captured = decoded;
		return '';
	});
	return { buf: holdback === 0 ? '' : text.slice(-holdback), scan: { visible, captured } };
}

/** How many characters of `tail` to hold back for the next
 * chunk. `tail` is the maximal trailing run of base64-ish
 * characters. Returns 0 when nothing should be held — the run
 * is flushed as ordinary output. */
function holdbackLength(tail: string): number {
	if (tail.length === 0) {
		return 0;
	}
	if (tail.length < HISTORY_MARKER.length) {
		// A short run is held only if it's genuinely the start of
		// the marker; otherwise it's a trailing base64 fragment
		// from real output and holding it would stall the stream.
		return HISTORY_MARKER.startsWith(tail) ? tail.length : 0;
	}
	// At or past the marker length: hold only when it opens
	// with the marker (marker + a payload that may still be
	// growing). A longer run that doesn't is plain output.
	return tail.startsWith(HISTORY_MARKER) ? tail.length : 0;
}

/** Decode a base64 payload to its command text. Returns `null`
 * when the payload doesn't decode (a stray marker word in real
 * output — collision costs one stale entry, not a crash). */
function decodePayload(payload: string): string | null {
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
