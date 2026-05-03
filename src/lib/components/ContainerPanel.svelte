<script lang="ts">
	import { container, containerStateLabel } from '../container.svelte';

	// Workspace shell popover, anchored above the status-bar pip.
	// Mirrors `ProjectComposePopover.svelte`: a fixed row of four
	// always-visible text buttons (Start / Stop / Recreate / Down)
	// where the slot is constant per state — only enablement
	// changes. Disabling rather than hiding keeps muscle memory
	// steady; tooltips spell out the literal `docker compose`
	// verb each button shells to.

	const state = $derived(container.state);
	const inFlight = $derived(container.inFlight);
	// "creating" while a setup is in flight is actually the optimistic
	// state — the real state arrives when the await resolves. Treat
	// `inFlight !== null` as the source of truth for disabling buttons
	// rather than the state enum.
	const busy = $derived(inFlight !== null);

	// `Start` is the user-visible verb across multiple states:
	// `setup` for absent/stopped/failed (writes compose then
	// `up -d --wait`), `resume` for paused (`unpause`). Centralise
	// the dispatch so the button stays in the same slot.
	function startAction(): Promise<void> {
		if (state === 'paused') {
			return container.resume();
		}
		return container.setup();
	}
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
	{:else if state === 'creating'}
		<p class="copy">Bringing up containers — this can take a few minutes the first time.</p>
	{:else if state === 'stopped'}
		<p class="copy">Containers exist but are stopped (someone ran <code>docker compose stop</code>?).</p>
	{:else if state === 'failed'}
		<p class="copy">One or more containers are unhealthy. See per-service detail below.</p>
	{/if}

	{#if state !== null}
		{@const canStart = state === 'absent' || state === 'stopped' || state === 'failed' || state === 'paused'}
		{@const canStop = state === 'running' || state === 'paused' || state === 'failed'}
		{@const canRecreate = state === 'running' || state === 'paused' || state === 'failed' || state === 'stopped'}
		{@const canDown = state !== 'absent'}
		<div class="actions" role="toolbar" aria-label="Workspace shell actions">
			<button
				type="button"
				class="action"
				disabled={busy || !canStart}
				title="Start the workspace shell (docker compose up -d, or unpause if paused). On first run this writes compose.yaml then waits for the container to be healthy."
				onclick={() => void startAction()}
			>
				Start
			</button>
			<button
				type="button"
				class="action"
				disabled={busy || !canStop}
				title="Stop the workspace shell (docker compose stop). Containers stay on the daemon for a fast restart, but in-container processes (LSPs, terminals, dev servers) are SIGTERMed — use docker compose pause from a terminal if you need to keep them in memory."
				onclick={() => container.stop()}
			>
				Stop
			</button>
			<button
				type="button"
				class="action"
				disabled={busy || !canRecreate}
				title="Recreate the workspace shell (docker compose up -d --force-recreate --pull always). Pulls a fresh moon-base image and recreates from current bound folders."
				onclick={() => container.rebuild()}
			>
				Recreate
			</button>
			<button
				type="button"
				class="action danger"
				disabled={busy || !canDown}
				title="Bring the workspace shell down (docker compose down). Removes containers and the network. Volumes are preserved."
				onclick={() => container.teardown()}
			>
				Down
			</button>
		</div>
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
	/* Match the per-folder popover (`ProjectComposePopover.svelte`):
	   four uniform text buttons that always render and flex to share
	   the row, disabled when not applicable to the current state. */
	.actions {
		display: flex;
		gap: 4px;
	}
	.action {
		flex: 1 1 0;
		min-width: 0;
		font: inherit;
		font-size: 12px;
		line-height: 1;
		background: var(--m-bg-overlay);
		color: var(--m-fg);
		border: 1px solid var(--m-border);
		border-radius: 4px;
		padding: 5px 6px;
		text-align: center;
		cursor: pointer;
	}
	.action:hover:not(:disabled) {
		background: var(--m-bg-1);
		border-color: var(--m-border-strong);
	}
	.action:disabled {
		opacity: 0.45;
		cursor: not-allowed;
	}
	.action.danger {
		color: var(--m-danger);
	}
	.action.danger:hover:not(:disabled) {
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
	.preview {
		margin-top: 2px;
	}
	.preview summary {
		cursor: pointer;
		color: var(--m-fg-muted);
		user-select: none;
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
