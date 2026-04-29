//! Reactive store for `docker compose logs -f` streams.
//!
//! One [`LogStream`] per open log tab. The Tauri side spawns the
//! child process and emits `compose_logs:line` per line +
//! `compose_logs:closed` once on exit; we buffer per stream and
//! mark the stream closed when the child finishes.
//!
//! Why a separate store from `bottomPanel`
//! ---------------------------------------
//!
//! Tabs in the bottom panel are deliberately thin (id, title,
//! kind, kind-specific keys). Putting log buffers + ANSI state
//! + follow-tail flags into the tab itself would couple the
//! panel chrome to a single tab kind — adding terminals (Phase 5)
//! would force the panel to know about both. Each tab kind owns
//! its content in a sibling store keyed on the tab id, and
//! `BottomPanel.svelte` switches on the kind to pick a body
//! component.
//!
//! Buffer cap
//! ----------
//!
//! Lines older than [`MAX_LINES_PER_STREAM`] are dropped from the
//! head of the buffer. Without a cap, a chatty service (e.g. a
//! verbose web server under load) would grow unbounded for as
//! long as the tab is open and eventually OOM the renderer.

import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import { SvelteMap } from 'svelte/reactivity';

import { bottomPanel, type LogTab } from './bottomPanel.svelte';
import { ipc } from './ipc';
import { formatError, type LogStreamClosed, type LogStreamLine } from './protocol';

const LINE_EVENT = 'compose_logs:line';
const CLOSED_EVENT = 'compose_logs:closed';

/** Hard cap on lines retained per stream. The viewer trims from
 * the head when this is exceeded — older log content scrolls off
 * but the live tail stays responsive. */
export const MAX_LINES_PER_STREAM = 5000;

/** Discriminated channel of a log line. Mirrors the backend's
 * `LogStreamLine.channel` string field but as a TS-only alias so
 * the renderer can switch on it directly. */
export type LogChannel = 'stdout' | 'stderr';

export type LogLine = {
	/** Monotonically increasing within a stream — used as the
	 * `{#each}` key so re-renders don't tear when lines arrive
	 * faster than 60fps. Resets when the buffer is cleared. */
	seq: number;
	channel: LogChannel;
	text: string;
};

export type LogStream = {
	streamId: string;
	folderPath: string;
	service: string;
	lines: LogLine[];
	/** Cleared on `closed` event so the viewer can show "stream
	 * ended" rather than pretend the tail is still live. */
	closed: boolean;
	/** Exit code from the supervisor's `wait()`, or `null` if
	 * the stream was closed by user action before the child
	 * exited cleanly. */
	closeCode: number | null;
	/** Error encountered while opening the stream (if any). The
	 * tab still mounts so the message is visible in the body
	 * rather than vanishing into a toast. */
	openError: string | null;
	/** Tail-follow flag. The viewer auto-scrolls on new lines
	 * when true; manual scroll-up flips this off so the user
	 * can read history without being yanked back. */
	follow: boolean;
};

class ComposeLogsStore {
	#streams = new SvelteMap<string, LogStream>();
	/** Per-stream sequence counter. Avoids fighting Svelte's
	 * reactivity by mutating an external counter for the `seq`
	 * field. */
	#nextSeq = new Map<string, number>();
	#unlisten: UnlistenFn[] = [];
	#runtimeWired = false;

	async wireRuntime(): Promise<void> {
		if (this.#runtimeWired) {
			return;
		}
		this.#runtimeWired = true;
		try {
			const onLine = await listen<LogStreamLine>(LINE_EVENT, (event) => {
				this.#appendLine(event.payload);
			});
			const onClosed = await listen<LogStreamClosed>(CLOSED_EVENT, (event) => {
				this.#markClosed(event.payload);
			});
			this.#unlisten.push(onLine, onClosed);
		} catch {
			// Event-bus bind failed — without it the buffer can
			// only show the open error, which still beats a silent
			// hang.
		}
	}

	/** Reactive lookup. `undefined` until [`open`] (or a manual
	 * placeholder seed) records a stream. */
	streamFor(streamId: string): LogStream | undefined {
		return this.#streams.get(streamId);
	}

	/**
	 * Open a stream for `folderPath`/`service` and add a log tab
	 * to the bottom panel. If a tab for this `(folderPath,
	 * service)` already exists (and its stream is still alive),
	 * focus it instead — duplicates rarely help and clutter the
	 * tab strip.
	 *
	 * Returns the resulting stream id. The bottom panel becomes
	 * visible as a side effect: the user clicked Logs to see
	 * something, hiding the destination would be silly.
	 */
	async open(folderPath: string, service: string): Promise<string> {
		const existing = bottomPanel.findLogTab(folderPath, service);
		if (existing) {
			bottomPanel.setActive(existing.id);
			bottomPanel.show();
			return existing.id;
		}

		bottomPanel.show();

		let streamId: string;
		try {
			streamId = await ipc.composeLogs.open(folderPath, service);
		} catch (err) {
			// Spawn failed (docker not running, project not up,
			// etc.). Mint a synthetic id and seed a closed stream
			// so the tab body can render the error message; the
			// user can close it once they've read it.
			streamId = `error-${cryptoRandomId()}`;
			this.#streams.set(streamId, {
				streamId,
				folderPath,
				service,
				lines: [],
				closed: true,
				closeCode: null,
				openError: formatError(err),
				follow: true,
			});
			this.#nextSeq.set(streamId, 0);
			bottomPanel.addTab(this.#tabFor(streamId, service, folderPath));
			return streamId;
		}

		this.#streams.set(streamId, {
			streamId,
			folderPath,
			service,
			lines: [],
			closed: false,
			closeCode: null,
			openError: null,
			follow: true,
		});
		this.#nextSeq.set(streamId, 0);
		bottomPanel.addTab(this.#tabFor(streamId, service, folderPath));
		return streamId;
	}

	/** Close the underlying child process and drop the tab. Safe
	 * to call on an already-closed stream — the backend ignores
	 * unknown ids and we just clean up local state. */
	async close(streamId: string): Promise<void> {
		const stream = this.#streams.get(streamId);
		if (!stream) {
			bottomPanel.closeTab(streamId);
			return;
		}
		try {
			if (!stream.closed && !stream.openError) {
				await ipc.composeLogs.close(streamId);
			}
		} catch {
			// Backend close failed (window torn down, daemon
			// gone). Nothing actionable; keep the local cleanup
			// going so the tab actually disappears.
		}
		this.#streams.delete(streamId);
		this.#nextSeq.delete(streamId);
		bottomPanel.closeTab(streamId);
	}

	/** Drop all buffered lines for a stream. The stream itself
	 * keeps running; the next line appended re-fills from
	 * sequence 0. */
	clear(streamId: string): void {
		const stream = this.#streams.get(streamId);
		if (!stream) {
			return;
		}
		this.#streams.set(streamId, { ...stream, lines: [] });
		this.#nextSeq.set(streamId, 0);
	}

	setFollow(streamId: string, follow: boolean): void {
		const stream = this.#streams.get(streamId);
		if (!stream || stream.follow === follow) {
			return;
		}
		this.#streams.set(streamId, { ...stream, follow });
	}

	#appendLine(payload: LogStreamLine): void {
		const stream = this.#streams.get(payload.stream_id);
		if (!stream) {
			return;
		}
		const seq = this.#nextSeq.get(payload.stream_id) ?? 0;
		this.#nextSeq.set(payload.stream_id, seq + 1);
		const line: LogLine = {
			seq,
			channel: payload.channel === 'stderr' ? 'stderr' : 'stdout',
			text: payload.text,
		};
		const lines =
			stream.lines.length >= MAX_LINES_PER_STREAM
				? [...stream.lines.slice(stream.lines.length - MAX_LINES_PER_STREAM + 1), line]
				: [...stream.lines, line];
		this.#streams.set(payload.stream_id, { ...stream, lines });
	}

	#markClosed(payload: LogStreamClosed): void {
		const stream = this.#streams.get(payload.stream_id);
		if (!stream) {
			return;
		}
		this.#streams.set(payload.stream_id, {
			...stream,
			closed: true,
			closeCode: payload.code,
		});
	}

	#tabFor(streamId: string, service: string, folderPath: string): LogTab {
		return {
			id: streamId,
			title: `Logs · ${service}`,
			kind: 'log',
			folderPath,
			service,
		};
	}
}

function cryptoRandomId(): string {
	if (typeof crypto !== 'undefined' && 'randomUUID' in crypto) {
		return crypto.randomUUID();
	}
	return Math.random().toString(36).slice(2);
}

export const composeLogs = new ComposeLogsStore();
