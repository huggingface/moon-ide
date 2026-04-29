<script lang="ts">
	import { projectCompose, projectComposeStateLabel } from '../projectCompose.svelte';

	// Per-folder popover anchored to the folder bar's compose
	// indicator. Mirrors `ContainerPanel.svelte` but scoped to a
	// single bound folder's compose project (its own
	// `docker-compose.yml`). Each folder bar instantiates one of
	// these on demand when the user clicks its indicator.

	type Props = {
		folderPath: string;
		folderName: string;
		onClose: () => void;
	};

	let { folderPath, folderName, onClose }: Props = $props();

	const snapshot = $derived(projectCompose.snapshotFor(folderPath));
	const inFlight = $derived(projectCompose.inFlightFor(folderPath));
	const lastError = $derived(projectCompose.errorFor(folderPath));
	const services = $derived(snapshot?.status.services ?? []);
	const state = $derived(snapshot?.status.state ?? null);
	const composeFile = $derived(snapshot?.compose_file ?? null);
	const projectName = $derived(snapshot?.project_name ?? null);
	// Mirror ContainerPanel: in-flight is the source of truth for
	// disabling buttons, not the state — `creating` while we're
	// awaiting `up -d --wait` is the optimistic state.
	const busy = $derived(inFlight !== undefined);

	// Human-readable banner shown above the action set while a
	// command is mid-flight. Without this the popover renders the
	// pre-action state (e.g. `failed` after a previous attempt)
	// with all buttons greyed, which reads as "stuck" rather than
	// "still working" — confusing when `compose up -d --wait`
	// genuinely takes minutes to settle. The state-specific
	// branches render under the banner so the service list etc.
	// stay visible for tracking progress.
	const busyCopy = $derived.by(() => {
		switch (inFlight) {
			case 'up':
				return 'Starting services — this can take a few minutes the first time.';
			case 'rebuild':
				return 'Rebuilding services — recreating containers and pulling fresh images.';
			case 'pause':
				return 'Pausing services…';
			case 'resume':
				return 'Resuming services…';
			case 'down':
				return 'Tearing down services…';
			default:
				return null;
		}
	});

	function handleKeydown(event: KeyboardEvent) {
		if (event.key === 'Escape') {
			event.preventDefault();
			onClose();
		}
	}
</script>

<svelte:window onkeydown={handleKeydown} />

<div class="panel" role="dialog" aria-label="Project services for {folderName}">
	<header>
		<span class="title">{folderName} services</span>
		<span
			class="status-text"
			class:state-running={state === 'running'}
			class:state-failed={state === 'failed'}
			class:state-paused={state === 'paused'}
		>
			{projectComposeStateLabel(snapshot)}
		</span>
	</header>

	{#if composeFile === null}
		<p class="copy">
			This folder has no compose file at its root. Add a <code>docker-compose.yml</code> or
			<code>compose.yaml</code> to manage services from here.
		</p>
	{:else}
		{#if lastError}
			<div class="error" role="alert">
				{lastError}
			</div>
		{/if}

		{#if busy && busyCopy}
			<p class="busy-copy">
				<span class="busy-spinner" aria-hidden="true"></span>
				{busyCopy}
			</p>
		{/if}

		<dl class="meta">
			<dt>Compose file</dt>
			<dd title={composeFile}>{composeFile}</dd>
			{#if projectName}
				<dt>Project name</dt>
				<dd>{projectName}</dd>
			{/if}
		</dl>

		{#if state === 'absent' || state === 'stopped'}
			<div class="actions">
				<button type="button" class="primary" disabled={busy} onclick={() => projectCompose.up(folderPath)}>
					{inFlight === 'up' ? 'Starting…' : 'Start services'}
				</button>
				{#if state === 'stopped'}
					<button type="button" class="danger" disabled={busy} onclick={() => projectCompose.down(folderPath)}>
						{inFlight === 'down' ? 'Tearing down…' : 'Tear down'}
					</button>
				{/if}
			</div>
		{:else if state === 'creating'}
			<p class="copy">Bringing up services — this can take a few minutes the first time.</p>
		{:else if state === 'running'}
			<div class="actions">
				<button type="button" disabled={busy} onclick={() => projectCompose.pause(folderPath)}>
					{inFlight === 'pause' ? 'Pausing…' : 'Pause'}
				</button>
				<button type="button" disabled={busy} onclick={() => projectCompose.rebuild(folderPath)}>
					{inFlight === 'rebuild' ? 'Rebuilding…' : 'Rebuild'}
				</button>
				<button type="button" class="danger" disabled={busy} onclick={() => projectCompose.down(folderPath)}>
					{inFlight === 'down' ? 'Tearing down…' : 'Stop services'}
				</button>
			</div>
		{:else if state === 'paused'}
			<div class="actions">
				<button type="button" class="primary" disabled={busy} onclick={() => projectCompose.resume(folderPath)}>
					{inFlight === 'resume' ? 'Resuming…' : 'Resume'}
				</button>
				<button type="button" class="danger" disabled={busy} onclick={() => projectCompose.down(folderPath)}>
					{inFlight === 'down' ? 'Tearing down…' : 'Stop services'}
				</button>
			</div>
		{:else if state === 'failed'}
			<p class="copy">One or more services are unhealthy. See per-service detail below.</p>
			<div class="actions">
				<button type="button" disabled={busy} onclick={() => projectCompose.rebuild(folderPath)}>
					{inFlight === 'rebuild' ? 'Rebuilding…' : 'Rebuild'}
				</button>
				<button type="button" class="danger" disabled={busy} onclick={() => projectCompose.down(folderPath)}>
					{inFlight === 'down' ? 'Tearing down…' : 'Tear down'}
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
	{/if}
</div>

<style>
	.panel {
		position: absolute;
		top: 100%;
		right: 0;
		margin-top: 4px;
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
	}
	.status-text {
		color: var(--m-fg-muted);
		font-variant-numeric: tabular-nums;
	}
	.status-text.state-running {
		color: var(--m-success);
	}
	.status-text.state-paused {
		color: var(--m-warning, var(--m-fg-muted));
	}
	.status-text.state-failed {
		color: var(--m-danger);
	}
	.copy {
		margin: 0;
		color: var(--m-fg-muted);
		line-height: 1.4;
	}
	/* In-flight banner. Pulses subtly so the user can tell at a
	   glance that work is happening; the message text says what
	   _kind_ of work, and the state-specific actions below stay
	   visible (greyed) so it's clear what they'll be able to do
	   when it lands. */
	.busy-copy {
		margin: 0;
		display: flex;
		align-items: center;
		gap: 8px;
		color: var(--m-fg);
		background: var(--m-bg-overlay);
		border: 1px solid var(--m-border);
		border-radius: 4px;
		padding: 6px 8px;
		line-height: 1.4;
		animation: busy-pulse 1.4s ease-in-out infinite;
	}
	.busy-spinner {
		width: 8px;
		height: 8px;
		border-radius: 50%;
		background: var(--m-warning, var(--m-fg-muted));
		flex-shrink: 0;
	}
	@keyframes busy-pulse {
		0%,
		100% {
			opacity: 1;
		}
		50% {
			opacity: 0.7;
		}
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
	.meta {
		margin: 0;
		display: grid;
		grid-template-columns: max-content 1fr;
		gap: 2px 8px;
		font-size: 11px;
	}
	.meta dt {
		color: var(--m-fg-subtle);
	}
	.meta dd {
		margin: 0;
		color: var(--m-fg-muted);
		font-family:
			ui-monospace,
			SFMono-Regular,
			SF Mono,
			Menlo,
			Consolas,
			monospace;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
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
	.services {
		margin-top: 2px;
	}
	.services summary {
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
</style>
