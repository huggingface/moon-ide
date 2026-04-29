<script lang="ts">
	import { tick } from 'svelte';
	import { composeLogs, MAX_LINES_PER_STREAM } from '../composeLogs.svelte';
	import type { LogTab } from '../bottomPanel.svelte';

	// Body component for a `kind: 'log'` bottom-panel tab. Renders
	// the buffered lines for this tab's stream id, auto-scrolls
	// when "follow tail" is on, and exposes Clear / Pause-follow /
	// Close controls in a compact toolbar.
	//
	// Lines come from `composeLogs.streamFor(tab.id)`, which is a
	// reactive `SvelteMap` lookup — Svelte re-runs the `$derived`
	// whenever the backend pushes a new line via the
	// `compose_logs:line` Tauri event.

	type Props = { tab: LogTab };
	let { tab }: Props = $props();

	const stream = $derived(composeLogs.streamFor(tab.id));
	const lines = $derived(stream?.lines ?? []);
	const closed = $derived(stream?.closed ?? false);
	const closeCode = $derived(stream?.closeCode ?? null);
	const openError = $derived(stream?.openError ?? null);
	const follow = $derived(stream?.follow ?? true);

	let bodyEl: HTMLDivElement | null = null;
	// `userScrolledAway` flips when the user manually scrolls up,
	// suspending follow until they scroll back to the bottom.
	// Without it, every new line yanks the viewport down even when
	// the user is reading earlier history.
	let userScrolledAway = $state(false);

	$effect(() => {
		// Re-trigger on every line append. `lines.length` is the
		// dependency the derived reads, so Svelte schedules this
		// effect for each batch.
		void lines.length;
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
		// 4px slop so a screen-end position with pixel rounding
		// still counts as "at the bottom" — otherwise the follow
		// flag would flicker off on its own scroll.
		const atBottom = bodyEl.scrollHeight - bodyEl.scrollTop - bodyEl.clientHeight < 4;
		userScrolledAway = !atBottom;
	}

	async function close() {
		await composeLogs.close(tab.id);
	}

	function clear() {
		composeLogs.clear(tab.id);
	}

	function toggleFollow() {
		composeLogs.setFollow(tab.id, !follow);
		// User explicitly asked to follow → unstick the scroll
		// suspension so the next line snaps to the bottom.
		if (!follow) {
			userScrolledAway = false;
		}
	}

	function fmtClosedReason(): string {
		if (closeCode === null) {
			return 'stream closed';
		}
		if (closeCode === 0) {
			return 'stream closed (exit 0)';
		}
		return `stream closed (exit ${closeCode})`;
	}
</script>

<div class="log-tab">
	<div class="toolbar">
		<span class="meta" title={tab.folderPath}>{tab.service}</span>
		{#if closed && !openError}
			<span class="closed-tag">{fmtClosedReason()}</span>
		{/if}
		<span class="spacer"></span>
		<button
			type="button"
			class="tb-btn"
			onclick={toggleFollow}
			disabled={closed}
			title={follow ? 'Pause auto-scroll' : 'Resume auto-scroll'}
			aria-pressed={follow}
		>
			{follow ? '⏸ Pause' : '▶ Follow'}
		</button>
		<button type="button" class="tb-btn" onclick={clear} title="Clear buffer">Clear</button>
		<button type="button" class="tb-btn" onclick={close} title="Close stream">Close</button>
	</div>
	{#if openError}
		<div class="error" role="alert">{openError}</div>
	{/if}
	<!-- tabindex=-1 keeps the log body focusable via click + region cycling
	     without putting a non-interactive element in the natural Tab order. -->
	<div class="body" bind:this={bodyEl} onscroll={onScroll} tabindex="-1" role="log" aria-live="polite">
		{#if lines.length === 0 && !openError}
			<p class="empty">{closed ? 'Stream ended with no output.' : 'Waiting for output…'}</p>
		{/if}
		{#each lines as line (line.seq)}
			<pre class="line" class:err={line.channel === 'stderr'}>{line.text}</pre>
		{/each}
	</div>
	{#if lines.length >= MAX_LINES_PER_STREAM}
		<div class="trim-note">
			Showing the last {MAX_LINES_PER_STREAM.toLocaleString()} lines — older history was trimmed.
		</div>
	{/if}
</div>

<style>
	.log-tab {
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
		font-family:
			ui-monospace,
			SFMono-Regular,
			SF Mono,
			Menlo,
			Consolas,
			monospace;
		font-size: 11px;
	}
	.closed-tag {
		color: var(--m-fg-subtle);
		font-size: 11px;
		font-style: italic;
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
	.tb-btn:hover:not(:disabled) {
		background: var(--m-bg-overlay);
		border-color: var(--m-border);
		color: var(--m-fg);
	}
	.tb-btn:disabled {
		opacity: 0.4;
		cursor: not-allowed;
	}
	.error {
		margin: 6px 8px;
		padding: 6px 8px;
		border: 1px solid var(--m-danger);
		border-radius: 4px;
		color: var(--m-danger);
		background: var(--m-bg-overlay);
		white-space: pre-wrap;
	}
	.body {
		flex: 1;
		min-height: 0;
		overflow-y: auto;
		overflow-x: auto;
		padding: 4px 8px;
		font-family:
			ui-monospace,
			SFMono-Regular,
			SF Mono,
			Menlo,
			Consolas,
			monospace;
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
	.line {
		margin: 0;
		white-space: pre;
		color: var(--m-fg);
	}
	.line.err {
		color: var(--m-danger);
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
