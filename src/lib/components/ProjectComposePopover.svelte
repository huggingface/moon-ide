<script lang="ts">
	import { isFailedService, type ServiceStatus } from '../protocol';
	import { composeLogs } from '../composeLogs.svelte';
	import { projectCompose, projectComposeStateLabel } from '../projectCompose.svelte';

	// Heuristic "this service is still settling" used to pulse
	// the row regardless of whether a moon-ide command happens to
	// be in flight: a service in `created` (blocked on
	// depends_on), `restarting` (failing + retrying), or
	// `running` with health `starting` (the healthcheck hasn't
	// flipped to healthy yet) is intrinsically transient — the
	// pulse helps the user spot the hold-up at a glance even if
	// they opened the popover after `up -d --wait` returned.
	function isWaitingService(svc: ServiceStatus): boolean {
		if (svc.raw_state === 'created') {
			return true;
		}
		if (svc.raw_state === 'restarting') {
			return true;
		}
		if (svc.raw_state === 'running' && svc.health === 'starting') {
			return true;
		}
		return false;
	}

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
	const inFlightService = $derived(projectCompose.inFlightServiceFor(folderPath));
	const lastError = $derived(projectCompose.errorFor(folderPath));
	const services = $derived(snapshot?.status.services ?? []);
	// Named `projectState` (not `state`) so the local doesn't
	// shadow the `$state` rune for tsgo, which otherwise resolves
	// `$state(true)` as `$<store>state` legacy auto-subscription.
	const projectState = $derived(snapshot?.status.state ?? null);
	const composeFile = $derived(snapshot?.compose_file ?? null);
	const projectName = $derived(snapshot?.project_name ?? null);
	// Mirror ContainerPanel: in-flight is the source of truth for
	// disabling buttons, not the state — `creating` while we're
	// awaiting `up -d --wait` is the optimistic state.
	const busy = $derived(inFlight !== undefined);

	// Local UI state for the services <details>. Default open so
	// the user sees the per-service rows on first popover open;
	// `bind:open` lets manual toggles stick across the 2s polling
	// re-renders (a stateless `open={...}` attribute would get
	// re-asserted on every snapshot refresh and re-collapse it).
	let servicesOpen = $state(true);

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
			case 'resume':
				return 'Resuming services…';
			case 'stop':
				return 'Stopping services…';
			case 'down':
				return 'Tearing down services — removing containers and network…';
			case 'service-start':
				return inFlightService ? `Starting ${inFlightService}…` : null;
			case 'service-stop':
				return inFlightService ? `Stopping ${inFlightService}…` : null;
			case 'service-restart':
				return inFlightService ? `Restarting ${inFlightService}…` : null;
			default:
				return null;
		}
	});

	// Per-row capability checks — mirror what `docker compose
	// {start,stop,restart} <svc>` accepts. Restart on a `created`
	// (never-started) container errors with "no such service
	// container", so we gate it; for those rows the user wants
	// project-level `up` instead.
	function canStart(svc: ServiceStatus): boolean {
		return svc.raw_state === 'created' || svc.raw_state === 'exited';
	}
	function canStop(svc: ServiceStatus): boolean {
		return svc.raw_state === 'running' || svc.raw_state === 'restarting';
	}
	function canRestart(svc: ServiceStatus): boolean {
		return svc.raw_state === 'running' || svc.raw_state === 'exited' || svc.raw_state === 'restarting';
	}

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
			class:state-running={projectState === 'running'}
			class:state-failed={projectState === 'failed'}
			class:state-paused={projectState === 'paused'}
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

		{#if projectState !== null}
			{@const canStart =
				projectState === 'absent' ||
				projectState === 'stopped' ||
				projectState === 'failed' ||
				projectState === 'paused'}
			{@const canStop = projectState === 'running' || projectState === 'paused' || projectState === 'failed'}
			{@const canRecreate =
				projectState === 'running' ||
				projectState === 'paused' ||
				projectState === 'failed' ||
				projectState === 'stopped'}
			{@const canDown = projectState !== 'absent'}
			<div class="actions" role="toolbar" aria-label="Project actions">
				<button
					type="button"
					class="action"
					disabled={busy || !canStart}
					title="Start services (docker compose up -d, or unpause if paused)"
					onclick={() =>
						projectState === 'paused' ? projectCompose.resume(folderPath) : projectCompose.up(folderPath)}
				>
					Start
				</button>
				<button
					type="button"
					class="action"
					disabled={busy || !canStop}
					title="Stop services (docker compose stop). Containers stay on the daemon for a fast restart."
					onclick={() => projectCompose.stop(folderPath)}
				>
					Stop
				</button>
				<button
					type="button"
					class="action"
					disabled={busy || !canRecreate}
					title="Recreate services (docker compose up -d --build --force-recreate). Pulls fresh images and recreates containers."
					onclick={() => projectCompose.rebuild(folderPath)}
				>
					Recreate
				</button>
				<button
					type="button"
					class="action danger"
					disabled={busy || !canDown}
					title="Bring services down (docker compose down). Removes containers and the network. Volumes are preserved."
					onclick={() => projectCompose.down(folderPath)}
				>
					Down
				</button>
			</div>
		{/if}

		{#if services.length > 0}
			<details class="services" bind:open={servicesOpen}>
				<summary>Services ({services.length})</summary>
				<ul>
					{#each services as svc (svc.name)}
						{@const targeted = inFlightService === svc.name}
						{@const waiting = isWaitingService(svc) || targeted}
						{@const failed = isFailedService(svc)}
						<li class:waiting class:failed>
							<span class="svc-marker" aria-hidden="true">
								{#if waiting}
									<span class="dot waiting-dot"></span>
								{:else if failed}
									<span class="dot failed-dot"></span>
								{:else}
									<span class="dot done-dot"></span>
								{/if}
							</span>
							<span class="svc-name">{svc.name}</span>
							<span class="svc-controls" aria-label="{svc.name} actions">
								{#if canStart(svc)}
									<button
										type="button"
										class="svc-btn"
										title="Start {svc.name}"
										aria-label="Start {svc.name}"
										disabled={busy}
										onclick={() => projectCompose.startService(folderPath, svc.name)}
									>
										▶
									</button>
								{/if}
								{#if canRestart(svc)}
									<button
										type="button"
										class="svc-btn"
										title="Restart {svc.name}"
										aria-label="Restart {svc.name}"
										disabled={busy}
										onclick={() => projectCompose.restartService(folderPath, svc.name)}
									>
										↻
									</button>
								{/if}
								{#if canStop(svc)}
									<button
										type="button"
										class="svc-btn"
										title="Stop {svc.name}"
										aria-label="Stop {svc.name}"
										disabled={busy}
										onclick={() => projectCompose.stopService(folderPath, svc.name)}
									>
										◼
									</button>
								{/if}
								<button
									type="button"
									class="svc-btn"
									title="Stream logs for {svc.name}"
									aria-label="Logs for {svc.name}"
									onclick={() => void composeLogs.open(folderPath, svc.name)}
								>
									≡
								</button>
							</span>
							<span class="svc-state svc-{svc.raw_state}" class:svc-bad-exit={failed}>
								<span class="state-text">
									{svc.raw_state}{svc.raw_state === 'exited' ? ` (${svc.exit_code})` : ''}
								</span>
								{#if svc.health === 'healthy'}
									<span class="health-ok" aria-label="healthy" title="healthy">✓</span>
								{:else if svc.health === 'starting'}
									<span class="health-warn">· starting</span>
								{:else if svc.health === 'unhealthy'}
									<span class="health-bad">· unhealthy</span>
								{:else if svc.health}
									<span class="health-text">· {svc.health}</span>
								{/if}
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
		/* Cap so the popover never extends past the viewport on
		   tall service lists. The internal services list takes
		   the slack via flex; everything above (header, errors,
		   meta, actions) stays anchored at the top. */
		max-height: calc(100vh - 64px);
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
	/* Project-level actions: a fixed row of four uniform text
	   buttons that always render. Disabling (rather than hiding)
	   the inapplicable ones keeps muscle memory steady — the
	   button you want is always in the same slot. Each button
	   flexes to share the row width equally so the popover reads
	   as a single toolbar regardless of available width.
	   Tooltips spell out the literal `docker compose` verb so the
	   precise effect is one hover away; the danger button keeps a
	   red tint as the only colour cue users need to slow down. */
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
	.services {
		margin-top: 2px;
		/* Take all remaining vertical space so the inner list can
		   scroll when long. The fixed-height items above keep
		   their natural size; only the services list flexes. */
		flex: 1;
		min-height: 0;
		display: flex;
		flex-direction: column;
	}
	.services[open] {
		overflow: hidden;
	}
	.services summary {
		cursor: pointer;
		color: var(--m-fg-muted);
		user-select: none;
		flex-shrink: 0;
	}
	.services ul {
		list-style: none;
		padding: 4px 0 0;
		margin: 0;
		display: flex;
		flex-direction: column;
		gap: 2px;
		overflow-y: auto;
		min-height: 0;
		flex: 1;
	}
	.services li {
		display: grid;
		grid-template-columns: 10px 1fr max-content max-content;
		align-items: center;
		gap: 6px;
		font-variant-numeric: tabular-nums;
		min-height: 22px;
	}
	/* Per-row action toolbar. Visible at low contrast by default,
	   pops on row hover to keep the list readable but
	   discoverable. Non-applicable buttons aren't rendered at
	   all — fewer greyed glyphs to look at, and nothing the user
	   could click but shouldn't. */
	.svc-controls {
		display: inline-flex;
		gap: 0;
		opacity: 0.35;
		transition: opacity 0.12s ease;
	}
	.services li:hover .svc-controls,
	.services li:focus-within .svc-controls {
		opacity: 1;
	}
	.svc-btn {
		font: inherit;
		font-size: 10px;
		line-height: 1;
		background: transparent;
		color: var(--m-fg-muted);
		border: 1px solid transparent;
		border-radius: 3px;
		padding: 2px 4px;
		cursor: pointer;
	}
	.svc-btn:hover:not(:disabled) {
		background: var(--m-bg-overlay);
		border-color: var(--m-border);
		color: var(--m-fg);
	}
	.svc-btn:disabled {
		opacity: 0.4;
		cursor: not-allowed;
	}
	.svc-marker {
		display: inline-flex;
		align-items: center;
		justify-content: center;
	}
	.svc-marker .dot {
		width: 6px;
		height: 6px;
		border-radius: 50%;
		background: var(--m-fg-subtle);
	}
	.svc-marker .done-dot {
		background: var(--m-fg-subtle);
	}
	.svc-marker .failed-dot {
		background: var(--m-danger);
	}
	.svc-marker .waiting-dot {
		background: var(--m-warning, var(--m-fg-muted));
	}
	/* Pulse the rows compose is still blocked on so the user can
	   spot the hold-up at a glance. Solid red rows are failures
	   that won't recover on their own — never pulse those. */
	.services li.waiting {
		animation: busy-pulse 1.4s ease-in-out infinite;
	}
	.svc-name {
		color: var(--m-fg);
	}
	.services li.waiting .svc-name {
		color: var(--m-warning, var(--m-fg));
	}
	.svc-state {
		color: var(--m-fg-muted);
		font-size: 11px;
	}
	.svc-running {
		color: var(--m-success);
	}
	/* Healthy is the steady-state default for `running`; render
	   it as a small check next to the state word rather than the
	   `· healthy` text so the right column stays compact. The
	   transient/bad health states keep explicit text in their
	   own colours because a row in those states needs attention
	   — the suffix is what tells the user something's off when
	   the row word still says "running". */
	.health-ok {
		color: var(--m-success);
		font-size: 11px;
		margin-left: 4px;
	}
	.health-warn {
		color: var(--m-warning, var(--m-fg-muted));
		font-size: 11px;
		margin-left: 4px;
	}
	.health-bad {
		color: var(--m-danger);
		font-size: 11px;
		margin-left: 4px;
	}
	.health-text {
		color: var(--m-fg-muted);
		font-size: 11px;
		margin-left: 4px;
	}
	.svc-paused,
	.svc-exited,
	.svc-created {
		color: var(--m-fg-muted);
	}
	.svc-restarting {
		color: var(--m-warning, var(--m-fg-muted));
	}
	/* Long-running services that exited with a non-zero code, an
	   unhealthy healthcheck, or `dead`/unknown — these are the
	   actionable problems. Plain `exited` (code 0) stays muted
	   because it's the expected end state for init containers. */
	.svc-bad-exit,
	.svc-dead {
		color: var(--m-danger);
	}
</style>
