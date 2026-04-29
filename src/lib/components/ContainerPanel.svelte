<script lang="ts">
	import { container, containerStateLabel } from '../container.svelte';

	// Phase 2.0 popover that pops above the status-bar pip. State-
	// dependent action set: each branch is small and the duplication
	// is intentional — the action vocabulary genuinely differs and
	// inlining it makes the affordances easier to reason about than
	// a giant button-config matrix would.

	const state = $derived(container.state);
	const inFlight = $derived(container.inFlight);
	const services = $derived(container.status?.services ?? []);
	// "creating" while a setup is in flight is actually the optimistic
	// state — the real state arrives when the await resolves. Treat
	// `inFlight !== null` as the source of truth for disabling buttons
	// rather than the state enum.
	const busy = $derived(inFlight !== null);
</script>

<div class="panel" role="dialog" aria-label="Container controls">
	<header>
		<span class="title">container</span>
		<span class="status-text" class:state-running={state === 'running'} class:state-failed={state === 'failed'}>
			{containerStateLabel(state)}
		</span>
	</header>

	{#if container.lastError}
		<div class="error" role="alert">
			{container.lastError}
		</div>
	{/if}

	{#if state === 'absent'}
		<p class="copy">No container yet for this workspace.</p>
		<div class="actions">
			<button type="button" class="primary" disabled={busy} onclick={() => container.setup()}>
				{inFlight === 'setup' ? 'Setting up…' : 'Set up'}
			</button>
		</div>
	{:else if state === 'creating'}
		<p class="copy">Bringing up containers — this can take a few minutes the first time.</p>
	{:else if state === 'running'}
		<div class="actions">
			<button type="button" disabled={busy} onclick={() => container.pause()}>
				{inFlight === 'pause' ? 'Pausing…' : 'Pause'}
			</button>
			<button type="button" disabled={busy} onclick={() => container.rebuild()}>
				{inFlight === 'rebuild' ? 'Rebuilding…' : 'Rebuild'}
			</button>
			<button type="button" class="danger" disabled={busy} onclick={() => container.teardown()}>
				{inFlight === 'teardown' ? 'Tearing down…' : 'Tear down'}
			</button>
		</div>
	{:else if state === 'paused'}
		<div class="actions">
			<button type="button" class="primary" disabled={busy} onclick={() => container.resume()}>
				{inFlight === 'resume' ? 'Resuming…' : 'Resume'}
			</button>
			<button type="button" class="danger" disabled={busy} onclick={() => container.teardown()}>
				{inFlight === 'teardown' ? 'Tearing down…' : 'Tear down'}
			</button>
		</div>
	{:else if state === 'stopped'}
		<p class="copy">Containers exist but are stopped (someone ran <code>docker compose stop</code>?).</p>
		<div class="actions">
			<button type="button" class="primary" disabled={busy} onclick={() => container.setup()}>
				{inFlight === 'setup' ? 'Starting…' : 'Start'}
			</button>
			<button type="button" class="danger" disabled={busy} onclick={() => container.teardown()}>
				{inFlight === 'teardown' ? 'Tearing down…' : 'Tear down'}
			</button>
		</div>
	{:else if state === 'failed'}
		<p class="copy">One or more containers are unhealthy. See per-service detail below.</p>
		<div class="actions">
			<button type="button" disabled={busy} onclick={() => container.rebuild()}>
				{inFlight === 'rebuild' ? 'Rebuilding…' : 'Rebuild'}
			</button>
			<button type="button" class="danger" disabled={busy} onclick={() => container.teardown()}>
				{inFlight === 'teardown' ? 'Tearing down…' : 'Tear down'}
			</button>
		</div>
	{/if}

	{#if services.length > 0}
		<details class="services" open={state === 'failed'}>
			<summary>Services ({services.length})</summary>
			<ul>
				{#each services as svc (svc.name)}
					<li>
						<span class="svc-name">{svc.name}</span>
						<span
							class="svc-state svc-{svc.raw_state}"
							class:svc-bad-exit={svc.raw_state === 'exited' && svc.exit_code !== 0}
						>
							{svc.raw_state}{svc.raw_state === 'exited' ? ` (${svc.exit_code})` : ''}{svc.health
								? ` · ${svc.health}`
								: ''}
						</span>
					</li>
				{/each}
			</ul>
		</details>
	{/if}

	<details class="preview">
		<summary onclick={() => container.togglePreview()}>Inspect compose.yaml</summary>
		{#if container.previewVisible}
			{#if container.previewError}
				<div class="error" role="alert">{container.previewError}</div>
			{:else if container.composePreview === null}
				<div class="copy">Loading…</div>
			{:else}
				<pre class="yaml">{container.composePreview}</pre>
			{/if}
		{/if}
	</details>
</div>

<style>
	.panel {
		position: absolute;
		bottom: 100%;
		right: 0;
		margin-bottom: 6px;
		min-width: 280px;
		max-width: 420px;
		background: var(--m-bg-2);
		border: 1px solid var(--m-border-strong);
		border-radius: 6px;
		padding: 10px 12px;
		box-shadow: 0 6px 24px rgba(0, 0, 0, 0.5);
		font-size: 12px;
		color: var(--m-fg);
		display: flex;
		flex-direction: column;
		gap: 8px;
		z-index: 20;
	}
	header {
		display: flex;
		align-items: baseline;
		justify-content: space-between;
		gap: 8px;
	}
	.title {
		font-weight: 600;
		text-transform: lowercase;
		letter-spacing: 0.02em;
	}
	.status-text {
		color: var(--m-fg-muted);
		font-variant-numeric: tabular-nums;
	}
	.status-text.state-running {
		color: var(--m-success);
	}
	.status-text.state-failed {
		color: var(--m-danger);
	}
	.copy {
		margin: 0;
		color: var(--m-fg-muted);
		line-height: 1.4;
	}
	.copy code {
		font-family:
			ui-monospace,
			SFMono-Regular,
			SF Mono,
			Menlo,
			Consolas,
			monospace;
		background: var(--m-bg-overlay);
		padding: 0 4px;
		border-radius: 3px;
		color: var(--m-fg);
	}
	.actions {
		display: flex;
		gap: 6px;
		flex-wrap: wrap;
	}
	.actions button {
		font: inherit;
		background: var(--m-bg-overlay);
		color: var(--m-fg);
		border: 1px solid var(--m-border);
		border-radius: 4px;
		padding: 4px 10px;
		cursor: pointer;
	}
	.actions button:hover:not(:disabled) {
		background: var(--m-bg-1);
		border-color: var(--m-border-strong);
	}
	.actions button:disabled {
		opacity: 0.5;
		cursor: progress;
	}
	.actions .primary {
		background: var(--m-accent);
		border-color: var(--m-accent);
		color: var(--m-accent-fg, #fff);
	}
	.actions .primary:hover:not(:disabled) {
		filter: brightness(1.1);
	}
	.actions .danger {
		color: var(--m-danger);
	}
	.actions .danger:hover:not(:disabled) {
		background: var(--m-danger);
		color: var(--m-bg);
		border-color: var(--m-danger);
	}
	.error {
		color: var(--m-danger);
		background: var(--m-bg-overlay);
		border: 1px solid var(--m-danger);
		border-radius: 4px;
		padding: 6px 8px;
		white-space: pre-wrap;
		max-height: 8em;
		overflow: auto;
	}
	.services,
	.preview {
		margin-top: 2px;
	}
	.services summary,
	.preview summary {
		cursor: pointer;
		color: var(--m-fg-muted);
		user-select: none;
	}
	.services ul {
		list-style: none;
		padding: 4px 0 0;
		margin: 0;
		display: flex;
		flex-direction: column;
		gap: 2px;
	}
	.services li {
		display: flex;
		justify-content: space-between;
		gap: 8px;
		font-variant-numeric: tabular-nums;
	}
	.svc-name {
		color: var(--m-fg);
	}
	.svc-state {
		color: var(--m-fg-muted);
		font-size: 11px;
	}
	.svc-running {
		color: var(--m-success);
	}
	.svc-paused,
	.svc-exited,
	.svc-created {
		color: var(--m-fg-muted);
	}
	.svc-restarting {
		color: var(--m-warning, var(--m-fg-muted));
	}
	/* Long-running services that exited with a non-zero code, or
	   anything `dead`/unknown — these are the actionable
	   problems. Plain `exited` (code 0) stays muted because it's
	   the expected end state for init containers. */
	.svc-bad-exit,
	.svc-dead {
		color: var(--m-danger);
	}
	.yaml {
		margin: 6px 0 0;
		padding: 8px;
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
		line-height: 1.4;
		max-height: 240px;
		overflow: auto;
		white-space: pre;
	}
</style>
