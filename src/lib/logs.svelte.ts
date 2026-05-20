//! Reactive store for the bottom-panel diagnostic logs view.
//!
//! Two producers feed into one buffer per source:
//!
//! 1. **Backend.** The Rust [`LogSink`] broadcasts every emit on the
//!    `logs:entry` Tauri event. Examples: LSP status transitions,
//!    server stderr, routing decisions. The store keeps a per-source
//!    ring of those entries and exposes a snapshot for the panel.
//! 2. **Frontend.** [`frontendLog`] writes directly into the same
//!    store without round-tripping through IPC. Examples:
//!    `editor.completion` ("Ctrl+Space pressed", "got N items"),
//!    `format-on-save` ("Ctrl+S in <file>", "no formatter
//!    available"). Going through IPC for these would add a tiny
//!    latency on every keystroke and would mean an empty panel when
//!    the backend isn't reachable — neither pays for itself.
//!
//! Sources are free-form keys with a `<area>.<sub-area>` convention;
//! the picker groups by prefix purely for display. New entries can
//! introduce new sources at any time — they appear in the picker on
//! the next pass.
//!
//! Per-source ring cap mirrors the backend's
//! ([`MAX_ENTRIES_PER_SOURCE`]). On overflow we drop the head, which
//! is fine for a debug pane: the user always wants the most recent
//! activity.

import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import { SvelteMap, SvelteSet } from 'svelte/reactivity';

import { ipc } from './ipc';
import { formatError, type LogEntry, type LogLevel } from './protocol';

/** Tauri event carrying one `LogEntry`. Mirrors the const in
 * `src-tauri/src/commands/logs.rs`. */
const ENTRY_EVENT = 'logs:entry';

/** Cap per source. Tuned to match the backend's
 * `MAX_PER_SOURCE` in `crates/moon-core/src/logs.rs`; the
 * lower limit "wins" overall. */
export const MAX_ENTRIES_PER_SOURCE = 2000;

/** Counter used to assign `seq` values to frontend-emitted entries.
 * Kept disjoint from the backend's positive seq space by sitting in
 * the negative range — the panel only uses `seq` as a stable
 * `{#each}` key and to break time-tied sort order, so collisions
 * between the two ranges don't matter. */
let frontendSeq = -1;

class DiagLogsStore {
	/** Per-source buffer. Order is emit order (older → newer). */
	#bySource = new SvelteMap<string, LogEntry[]>();
	/** Union of every source ever seen, so the picker can list a
	 * source even if the user hasn't opened it yet. Backend
	 * sources are seeded from `logs_sources` on first init;
	 * subsequent ones grow on every emit. */
	#sources = new SvelteSet<string>();
	#unlisten: UnlistenFn | null = null;
	#started = false;
	/** Live entries that arrived from the Tauri pump and haven't
	 * been folded into [`#bySource`] yet. Drained on the next
	 * flush tick — see [`#scheduleFlush`]. Snapshot backfill and
	 * frontend-emitted entries take the direct [`#append`] path
	 * instead so the user sees their own log lines / a freshly
	 * opened backend source immediately. */
	#pending: LogEntry[] = [];
	#flushScheduled = false;

	/** Start the live-event subscription + seed the known-source
	 * list. Idempotent: repeated calls during dev hot-reloads are
	 * no-ops. */
	async start(): Promise<void> {
		if (this.#started) {
			return;
		}
		this.#started = true;
		this.#unlisten = await listen<LogEntry>(ENTRY_EVENT, (event) => {
			this.#enqueue(event.payload);
		});
		try {
			const sources = await ipc.logs.sources();
			for (const source of sources) {
				this.#sources.add(source);
			}
		} catch {
			// Backend not ready (e.g. preboot) — the picker just
			// shows whatever shows up live.
		}
	}

	async stop(): Promise<void> {
		this.#started = false;
		const unlisten = this.#unlisten;
		this.#unlisten = null;
		if (unlisten) {
			unlisten();
		}
	}

	/** All sources known to the store, sorted alphabetically.
	 * Reactive — Svelte re-derives the picker when new sources
	 * appear via either pump or [`frontendLog`]. */
	get sources(): readonly string[] {
		return [...this.#sources].toSorted((a, b) => a.localeCompare(b));
	}

	/** Entries currently held for `source`. Empty array (not
	 * `null`) when no entries yet, so the panel can render an
	 * empty state without an extra branch. */
	entriesFor(source: string): readonly LogEntry[] {
		return this.#bySource.get(source) ?? EMPTY;
	}

	/** Fetch and merge whatever backfill the backend has for
	 * `source`. Called lazily the first time the panel opens a
	 * source so a freshly-opened tab shows recent history
	 * instead of waiting for the next live entry. Safe to call
	 * repeatedly — duplicate seqs are merged out. */
	async loadSnapshot(source: string): Promise<void> {
		try {
			const entries = await ipc.logs.snapshot(source);
			for (const entry of entries) {
				this.#append(entry);
			}
		} catch (err) {
			this.#append({
				source: 'logs',
				level: 'error',
				message: `snapshot(${source}) failed: ${formatError(err)}`,
				tsMs: Date.now(),
				seq: frontendSeq--,
			});
		}
	}

	/** Drop every entry for `source` locally **and** on the
	 * backend. Used by the panel toolbar's Clear button. We
	 * keep the source in the picker so the user doesn't lose
	 * the tab they're looking at; the next emit re-creates
	 * the buffer. */
	async clear(source: string): Promise<void> {
		this.#bySource.set(source, []);
		try {
			await ipc.logs.clear(source);
		} catch {
			// Best-effort. Local clear already happened.
		}
	}

	/** Append a frontend-emitted entry. Same fan-out as the
	 * Tauri event listener. Exposed publicly so [`frontendLog`]
	 * can route through a single ingest path. */
	injectLocalEntry(entry: LogEntry): void {
		this.#append(entry);
	}

	/** Live-pump fast path. Queues `entry` for the next flush tick.
	 *
	 * A noisy LSP server (e.g. rust-analyzer panic-recovering
	 * mid-reindex after a big branch switch) can spew thousands
	 * of stderr lines per second, each one a Tauri event. Folding
	 * them in one-by-one means thousands of array clones and
	 * SvelteMap mutations per second, which pins the webview's
	 * JS thread and shows up as a frontend freeze. Coalescing
	 * across one event-loop tick collapses that into one
	 * allocation + one reactive set per affected source per tick.
	 *
	 * Snapshot backfill and frontend-emitted entries go through
	 * [`#append`] instead — they're already low-volume and the
	 * user expects them to appear synchronously. */
	#enqueue(entry: LogEntry): void {
		this.#pending.push(entry);
		this.#scheduleFlush();
	}

	#scheduleFlush(): void {
		if (this.#flushScheduled) {
			return;
		}
		this.#flushScheduled = true;
		// `setTimeout(0)` (not `queueMicrotask`) so each batch
		// yields to the browser's render pipeline between bursts.
		// Tauri delivers events as individual JS turns; microtasks
		// would still flush per-event because they drain between
		// each turn. A macrotask collapses everything queued in
		// the current event-loop iteration.
		setTimeout(() => {
			this.#flushScheduled = false;
			const batch = this.#pending;
			if (batch.length === 0) {
				return;
			}
			this.#pending = [];
			this.#applyBatch(batch);
		}, 0);
	}

	#applyBatch(batch: LogEntry[]): void {
		// Group additions by source so each source pays one
		// SvelteMap set regardless of how many entries from the
		// burst belong to it.
		const grouped = new Map<string, LogEntry[]>();
		for (const entry of batch) {
			this.#sources.add(entry.source);
			const list = grouped.get(entry.source);
			if (list) {
				list.push(entry);
			} else {
				grouped.set(entry.source, [entry]);
			}
		}
		for (const [source, additions] of grouped) {
			const current = this.#bySource.get(source) ?? EMPTY;
			const next = mergeEntries(current, additions);
			if (next === null) {
				continue;
			}
			this.#bySource.set(source, next);
		}
	}

	/** Append a single entry directly into [`#bySource`]. Used by
	 * snapshot backfill (`loadSnapshot`) and frontend-emitted
	 * entries (`injectLocalEntry`) where one-allocation-per-call
	 * is fine and the caller wants the result visible without
	 * waiting for the live-pump flush tick. */
	#append(entry: LogEntry): void {
		this.#sources.add(entry.source);
		const current = this.#bySource.get(entry.source) ?? EMPTY;
		const next = mergeEntries(current, [entry]);
		if (next === null) {
			return;
		}
		this.#bySource.set(entry.source, next);
	}
}

/** Fold `additions` into `current`, returning a new array sorted
 * by (`tsMs`, `seq`). Returns `null` when every addition is
 * already in the buffer (a snapshot fetch racing the live stream
 * is the only realistic shape) — the caller skips the `set` in
 * that case.
 *
 * Fast path: when the buffer plus the additions form a single
 * monotonically increasing seq run (live-pump common case), we
 * concat + truncate without any dedup scan. The slow path covers
 * out-of-order arrivals (a snapshot fetch returning entries the
 * user emitted while the request was in flight) by Set-dedup and
 * a single `sort` at the end. */
function mergeEntries(current: readonly LogEntry[], additions: LogEntry[]): LogEntry[] | null {
	if (additions.length === 0) {
		return null;
	}
	let monotonic = true;
	let prevSeq = current.length > 0 ? current[current.length - 1]!.seq : Number.NEGATIVE_INFINITY;
	let prevTs = current.length > 0 ? current[current.length - 1]!.tsMs : Number.NEGATIVE_INFINITY;
	for (const a of additions) {
		// Backend seqs grow positively per-process; frontend ones
		// decrease into the negatives from `frontendSeq`. Either
		// way, a strictly increasing `seq` across the run means no
		// (seq, source) collision is possible against the buffer
		// or among the additions themselves — so the O(N) dedup
		// scan is dead code on this path.
		if (a.seq <= prevSeq || a.tsMs < prevTs) {
			monotonic = false;
			break;
		}
		prevSeq = a.seq;
		prevTs = a.tsMs;
	}
	if (monotonic) {
		const merged = current.length === 0 ? additions.slice() : [...current, ...additions];
		return merged.length > MAX_ENTRIES_PER_SOURCE ? merged.slice(merged.length - MAX_ENTRIES_PER_SOURCE) : merged;
	}
	const seen = new Set<number>();
	for (const e of current) {
		seen.add(e.seq);
	}
	let added = 0;
	const merged: LogEntry[] = current.slice();
	for (const a of additions) {
		if (seen.has(a.seq)) {
			continue;
		}
		seen.add(a.seq);
		merged.push(a);
		added += 1;
	}
	if (added === 0) {
		return null;
	}
	merged.sort((x, y) => x.tsMs - y.tsMs || x.seq - y.seq);
	return merged.length > MAX_ENTRIES_PER_SOURCE ? merged.slice(merged.length - MAX_ENTRIES_PER_SOURCE) : merged;
}

const EMPTY: readonly LogEntry[] = Object.freeze([]);

export const diagLogs = new DiagLogsStore();

/** Emit a frontend-side log entry. Cheap — no IPC, just a local
 * append. The panel picks it up immediately if the matching tab
 * is open, and the source appears in the picker for any other
 * tab the user opens. */
export function frontendLog(source: string, level: LogLevel, message: string): void {
	diagLogs.injectLocalEntry({
		source,
		level,
		message,
		tsMs: Date.now(),
		seq: frontendSeq--,
	});
}
