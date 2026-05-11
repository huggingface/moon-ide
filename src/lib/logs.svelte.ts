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

	/** Start the live-event subscription + seed the known-source
	 * list. Idempotent: repeated calls during dev hot-reloads are
	 * no-ops. */
	async start(): Promise<void> {
		if (this.#started) {
			return;
		}
		this.#started = true;
		this.#unlisten = await listen<LogEntry>(ENTRY_EVENT, (event) => {
			this.#append(event.payload);
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

	#append(entry: LogEntry): void {
		this.#sources.add(entry.source);
		const current = this.#bySource.get(entry.source);
		if (!current) {
			this.#bySource.set(entry.source, [entry]);
			return;
		}
		// Dedup by seq: backend pump + a `loadSnapshot` racing on
		// the same source can otherwise insert the same entry
		// twice. Cheap because the buffer is bounded.
		if (current.some((e) => e.seq === entry.seq)) {
			return;
		}
		// SvelteMap reactivity fires on `.set` but not on mutating
		// the value array in place, so every append produces a new
		// array and re-`set`s it. Matches the pattern in
		// `composeLogs.svelte.ts`. The fast path slices in O(1)
		// when the new entry sorts after the tail (live pump);
		// late-arriving snapshot backfill slots in by time order.
		const last = current[current.length - 1];
		let next: LogEntry[];
		if (last && entry.tsMs >= last.tsMs) {
			next =
				current.length >= MAX_ENTRIES_PER_SOURCE
					? [...current.slice(current.length - MAX_ENTRIES_PER_SOURCE + 1), entry]
					: [...current, entry];
		} else {
			const idx = findInsertIndex(current, entry);
			next = [...current.slice(0, idx), entry, ...current.slice(idx)];
			if (next.length > MAX_ENTRIES_PER_SOURCE) {
				next = next.slice(next.length - MAX_ENTRIES_PER_SOURCE);
			}
		}
		this.#bySource.set(entry.source, next);
	}
}

const EMPTY: readonly LogEntry[] = Object.freeze([]);

function findInsertIndex(buf: LogEntry[], entry: LogEntry): number {
	for (let i = buf.length - 1; i >= 0; i--) {
		const cur = buf[i];
		if (cur === undefined) {
			continue;
		}
		if (entry.tsMs >= cur.tsMs) {
			return i + 1;
		}
	}
	return 0;
}

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
