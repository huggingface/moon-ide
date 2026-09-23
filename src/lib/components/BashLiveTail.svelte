<script lang="ts">
	// Live tail of a detached background process (ADR 0085), rendered
	// inside the spawning `bash` row's expanded body. Polls
	// `coder_read_background_process` — the same registry the model's
	// `read_process` reads — every 1.2s while the process runs, and
	// fetches once for a settled one (the registry retains settled
	// entries and their logs for the session's lifetime). Degrades to
	// nothing when the registry can't answer (IDE restarted since the
	// spawn — the in-memory handle is gone, only the persisted row
	// remains).
	import { ipc } from '../ipc';
	import type { BackgroundProcessSnapshot } from '../protocol';

	type Props = {
		sessionId: string;
		bgId: string;
		/** Whether the row believes the process is still running —
		 *  drives the polling loop; the settlement event flips it. */
		running: boolean;
	};

	let { sessionId, bgId, running }: Props = $props();

	let snapshot = $state<BackgroundProcessSnapshot | null>(null);
	let lost = $state(false);
	let stopping = $state(false);

	async function fetchOnce(): Promise<void> {
		try {
			snapshot = await ipc.coder.readBackgroundProcess(sessionId, bgId);
			lost = false;
		} catch {
			// Registry miss (restart) or unmounted session — stop
			// claiming liveness, keep whatever we last showed.
			lost = true;
		}
	}

	$effect(() => {
		// Re-arm on identity change; poll only while running.
		void sessionId;
		void bgId;
		void fetchOnce();
		if (!running) {
			return;
		}
		const timer = setInterval(() => void fetchOnce(), 1200);
		return () => clearInterval(timer);
	});

	async function stop(): Promise<void> {
		stopping = true;
		try {
			await ipc.coder.stopBackgroundProcess(sessionId, bgId);
			await fetchOnce();
		} catch {
			lost = true;
		} finally {
			stopping = false;
		}
	}
</script>

{#if snapshot !== null && (snapshot.tail.length > 0 || running)}
	<div class="bg-tail" class:live={running && !lost}>
		<div class="bg-tail-bar">
			<span class="bg-tail-label">
				{running && !lost ? 'live output' : 'output'}
			</span>
			{#if running && !lost}
				<button type="button" class="bg-stop" disabled={stopping} onclick={() => void stop()}> stop </button>
			{/if}
		</div>
		{#if snapshot.tail.length > 0}
			<pre class="bg-tail-stream">{snapshot.tail}</pre>
		{:else}
			<div class="bg-tail-empty">no output yet…</div>
		{/if}
	</div>
{/if}

<style>
	.bg-tail {
		margin-top: 4px;
		display: flex;
		flex-direction: column;
		gap: 2px;
	}
	.bg-tail-bar {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 8px;
	}
	.bg-tail-label {
		font-size: 10px;
		color: var(--m-fg-subtle);
		text-transform: uppercase;
		letter-spacing: 0.04em;
	}
	.bg-tail.live .bg-tail-label {
		color: var(--m-warning, var(--m-fg-muted));
	}
	.bg-stop {
		font: inherit;
		font-size: 10px;
		line-height: 1;
		background: transparent;
		color: var(--m-danger);
		border: 1px solid var(--m-border);
		border-radius: 3px;
		padding: 2px 6px;
		cursor: pointer;
	}
	.bg-stop:hover:not(:disabled) {
		background: var(--m-danger);
		color: var(--m-bg);
		border-color: var(--m-danger);
	}
	.bg-stop:disabled {
		opacity: 0.5;
		cursor: not-allowed;
	}
	/* Same look as the panel's bash stdout stream (those classes are
	   scoped to CoderPanel, so restated here); height capped so a
	   chatty build doesn't take over the transcript — the tail is
	   the newest 8 kB anyway. */
	.bg-tail-stream {
		margin: 0;
		padding: 6px 8px;
		background: var(--m-bg-1);
		border: 1px solid var(--m-border);
		border-radius: 4px;
		font-family:
			ui-monospace,
			SFMono-Regular,
			SF Mono,
			Menlo,
			Consolas,
			monospace;
		font-size: 11px;
		line-height: 1.45;
		white-space: pre-wrap;
		word-break: break-word;
		color: var(--m-fg);
		max-height: 220px;
		overflow-y: auto;
	}
	.bg-tail-empty {
		font-size: 11px;
		color: var(--m-fg-subtle);
		font-style: italic;
	}
</style>
