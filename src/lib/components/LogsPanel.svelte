<script lang="ts">
	import { tick } from 'svelte';
	import { bottomPanel, type DiagTab } from '../bottomPanel.svelte';
	import { diagLogs, MAX_ENTRIES_PER_SOURCE } from '../logs.svelte';
	import { workspace } from '../state.svelte';
	import type { LogLevel } from '../protocol';

	/** Diagnostic-logs sources for live LSP brokers are tagged
	 * `lsp.<language_id>` (matches the convention in
	 * `moon_core::lsp::broker::log_source_for`). The "Restart"
	 * button only makes sense for those tabs; we hide it on
	 * `format-on-save`, `editor.completion`, etc. */
	const LSP_SOURCE_PREFIX = 'lsp.';

	// Body component for a `kind: 'diag'` bottom-panel tab. Renders
	// one diagnostic source from the unified `diagLogs` store —
	// either a backend bucket (`lsp.typescript`, `lsp.rust`, …) or
	// a frontend-emitted one (`editor.completion`,
	// `format-on-save`, …). The store handles backfill and live
	// stream; this component is purely view.
	//
	// On mount we request the backend snapshot for the source so a
	// freshly-opened tab carries history forward. Live entries
	// flow through the Tauri pump → `diagLogs.start()` listener
	// and re-render this view by reading from the same store.

	type Props = { tab: DiagTab };
	let { tab }: Props = $props();

	const entries = $derived(diagLogs.entriesFor(tab.source));
	const lspLanguageId = $derived(
		tab.source.startsWith(LSP_SOURCE_PREFIX) ? tab.source.slice(LSP_SOURCE_PREFIX.length) : null,
	);
	let restartInFlight = $state(false);

	let bodyEl: HTMLDivElement | null = null;
	let userScrolledAway = $state(false);
	let follow = $state(true);

	$effect(() => {
		// One-shot per-source backfill load. Cheap on re-runs
		// thanks to seq-based dedup in the store.
		void diagLogs.loadSnapshot(tab.source);
	});

	$effect(() => {
		void entries.length;
		if (!follow || userScrolledAway) {
			return;
		}
		void tick().then(() => {
			if (bodyEl) {
				bodyEl.scrollTop = bodyEl.scrollHeight;
			}
		});
	});

	function onScroll() {
		if (!bodyEl) {
			return;
		}
		// 4px slop so pixel rounding at the very end doesn't toggle
		// the follow suspension on its own scroll.
		const atBottom = bodyEl.scrollHeight - bodyEl.scrollTop - bodyEl.clientHeight < 4;
		userScrolledAway = !atBottom;
	}

	async function clear() {
		await diagLogs.clear(tab.source);
	}

	async function restart() {
		if (lspLanguageId === null || restartInFlight) {
			return;
		}
		restartInFlight = true;
		try {
			await workspace.restartLsp(lspLanguageId);
		} finally {
			restartInFlight = false;
		}
	}

	function toggleFollow() {
		follow = !follow;
		if (follow) {
			userScrolledAway = false;
		}
	}

	function close() {
		bottomPanel.closeTab(tab.id);
	}

	function fmtTime(tsMs: number): string {
		const d = new Date(tsMs);
		const hh = d.getHours().toString().padStart(2, '0');
		const mm = d.getMinutes().toString().padStart(2, '0');
		const ss = d.getSeconds().toString().padStart(2, '0');
		const ms = d.getMilliseconds().toString().padStart(3, '0');
		return `${hh}:${mm}:${ss}.${ms}`;
	}

	function levelClass(level: LogLevel): string {
		return `lv-${level}`;
	}
</script>

<div class="diag-tab">
	<div class="toolbar">
		<span class="meta">{tab.source}</span>
		<span class="meta count">{entries.length} entr{entries.length === 1 ? 'y' : 'ies'}</span>
		<span class="spacer"></span>
		<button
			type="button"
			class="tb-btn"
			onclick={toggleFollow}
			title={follow ? 'Pause auto-scroll' : 'Resume auto-scroll'}
			aria-pressed={follow}
		>
			{follow ? '⏸ Pause' : '▶ Follow'}
		</button>
		{#if lspLanguageId !== null}
			<button
				type="button"
				class="tb-btn"
				onclick={restart}
				disabled={restartInFlight}
				title="Tear down the {lspLanguageId} language server and let the next request re-spawn it"
			>
				{restartInFlight ? 'Restarting…' : 'Restart'}
			</button>
		{/if}
		<button type="button" class="tb-btn" onclick={clear} title="Clear buffer">Clear</button>
		<button type="button" class="tb-btn" onclick={close} title="Close tab">Close</button>
	</div>
	<div class="body" bind:this={bodyEl} onscroll={onScroll} tabindex="-1" role="log" aria-live="polite">
		{#if entries.length === 0}
			<p class="empty">No entries yet. Waiting for output…</p>
		{/if}
		{#each entries as entry (entry.seq)}
			<div class="row {levelClass(entry.level)}">
				<span class="ts">{fmtTime(entry.tsMs)}</span>
				<span class="lv">{entry.level}</span>
				<pre class="msg">{entry.message}</pre>
			</div>
		{/each}
	</div>
	{#if entries.length >= MAX_ENTRIES_PER_SOURCE}
		<div class="trim-note">
			Showing the last {MAX_ENTRIES_PER_SOURCE.toLocaleString()} entries — older history was trimmed.
		</div>
	{/if}
</div>

<style>
	.diag-tab {
		display: flex;
		flex-direction: column;
		min-height: 0;
		flex: 1;
	}
	.toolbar {
		display: flex;
		align-items: center;
		gap: 6px;
		padding: 4px 8px;
		border-bottom: 1px solid var(--m-border);
		background: var(--m-bg-1);
		flex-shrink: 0;
	}
	.meta {
		color: var(--m-fg-muted);
		font-family: ui-monospace, SFMono-Regular, 'SF Mono', Menlo, Consolas, monospace;
		font-size: 11px;
	}
	.meta.count {
		color: var(--m-fg-subtle);
	}
	.spacer {
		flex: 1;
	}
	.tb-btn {
		font: inherit;
		font-size: 11px;
		line-height: 1;
		background: transparent;
		color: var(--m-fg-muted);
		border: 1px solid transparent;
		border-radius: 3px;
		padding: 3px 8px;
		cursor: pointer;
	}
	.tb-btn:hover {
		background: var(--m-bg-overlay);
		border-color: var(--m-border);
		color: var(--m-fg);
	}
	.tb-btn:disabled {
		opacity: 0.5;
		cursor: default;
		background: transparent;
		border-color: transparent;
	}
	.body {
		flex: 1;
		min-height: 0;
		overflow-y: auto;
		overflow-x: auto;
		padding: 4px 8px;
		font-family: ui-monospace, SFMono-Regular, 'SF Mono', Menlo, Consolas, monospace;
		font-size: 12px;
		line-height: 1.4;
		background: var(--m-bg);
	}
	.body:focus {
		outline: 1px solid var(--m-accent);
		outline-offset: -1px;
	}
	.empty {
		margin: 0;
		color: var(--m-fg-muted);
	}
	.row {
		display: grid;
		grid-template-columns: 88px 56px 1fr;
		gap: 8px;
		align-items: start;
		padding: 1px 0;
	}
	.ts {
		color: var(--m-fg-subtle);
		font-variant-numeric: tabular-nums;
	}
	.lv {
		text-transform: uppercase;
		font-size: 10px;
		letter-spacing: 0.5px;
		padding: 1px 4px;
		border-radius: 2px;
		text-align: center;
		align-self: center;
		color: var(--m-bg-1);
		background: var(--m-fg-muted);
		line-height: 1.2;
	}
	.row.lv-debug .lv {
		background: var(--m-fg-subtle);
	}
	.row.lv-info .lv {
		background: var(--m-fg-muted);
	}
	.row.lv-warn .lv {
		background: var(--m-warning, #d8a657);
		color: #1d2021;
	}
	.row.lv-error .lv {
		background: var(--m-danger, #e07474);
		color: #1d2021;
	}
	.row.lv-warn .msg {
		color: var(--m-warning, #d8a657);
	}
	.row.lv-error .msg {
		color: var(--m-danger, #e07474);
	}
	.msg {
		margin: 0;
		white-space: pre-wrap;
		word-break: break-word;
		color: var(--m-fg);
	}
	.trim-note {
		flex-shrink: 0;
		padding: 2px 8px;
		font-size: 10px;
		color: var(--m-fg-subtle);
		background: var(--m-bg-1);
		border-top: 1px solid var(--m-border);
	}
</style>
