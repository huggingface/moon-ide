<script lang="ts">
	import { workspace } from '../state.svelte';
	import { slack } from '../slack.svelte';
	import { coder } from '../coder.svelte';
	import { container, containerStateLabel } from '../container.svelte';
	import ContainerPanel from './ContainerPanel.svelte';
	import TerminalLauncher from './TerminalLauncher.svelte';
	import ThemePicker from './ThemePicker.svelte';
	import { lspLanguageFor } from '../editor/lspLanguage';

	let themePicker: ThemePicker | undefined = $state();
	let containerWrap: HTMLDivElement | undefined = $state();

	// Error / warning counts for the current file. Info and hint
	// diagnostics are intentionally omitted from the pill: tsserver
	// emits plenty of low-priority hints (unused imports, deprecated
	// APIs) that would otherwise light the pill up on most buffers
	// and desensitise the user to real problems.
	const activeFileDiagnostics = $derived(
		workspace.activeFile ? (workspace.diagnostics.get(workspace.activeFile.path) ?? []) : [],
	);
	const errorCount = $derived(activeFileDiagnostics.filter((d) => d.severity === 'error').length);
	const warnCount = $derived(activeFileDiagnostics.filter((d) => d.severity === 'warning').length);

	// Per-language LSP availability pill. We show it only when the
	// active file maps to a language we have a server wired up for
	// (so a Markdown buffer doesn't flash a "typescript not
	// available" message) AND the server's state is notable —
	// `running` is the happy path; we skip the pill there. Statuses
	// we surface:
	//   - `notavailable` — nudge the user to install the binary
	//   - `starting` — ~3-5s spinner while tsserver boots
	//   - `crashed` — last run died; next file open will retry
	//   - `stopped` — shutdown fired (during workspace close)
	const activeLanguage = $derived(workspace.activeFile ? lspLanguageFor(workspace.activeFile.path) : null);
	const activeLspStatus = $derived(activeLanguage ? (workspace.lspStatuses.get(activeLanguage) ?? null) : null);
	const showLspStatus = $derived(activeLspStatus !== null && activeLspStatus.status !== 'running');
	const lspStatusLabel = $derived.by(() => {
		const s = activeLspStatus;
		if (!s) {
			return '';
		}
		switch (s.status) {
			case 'notavailable':
				return `${s.languageId}: not available`;
			case 'starting':
				return `${s.languageId}: starting…`;
			case 'crashed':
				return `${s.languageId}: crashed`;
			case 'stopped':
				return `${s.languageId}: stopped`;
			default:
				return s.languageId;
		}
	});
	const lspStatusTitle = $derived.by(() => {
		const s = activeLspStatus;
		if (!s) {
			return '';
		}
		if (s.status === 'notavailable') {
			return s.detail ?? `Install a language server for ${s.languageId}`;
		}
		return s.detail ?? lspStatusLabel;
	});

	// Optimistic state during the two long-running ops (setup,
	// rebuild) so the pip transitions immediately rather than
	// staying on the previous glyph for a few minutes while
	// `up -d --wait` is in flight. Pause / resume / teardown are
	// quick enough that flicker isn't worth the extra branching.
	const effectiveState = $derived(
		container.inFlight === 'setup' || container.inFlight === 'rebuild' ? 'creating' : container.state,
	);

	// F6 cycle can land on the status bar; focus the theme picker
	// (the right-most interactive control). If we add more controls
	// here, switch to a generic "first focusable" lookup like
	// Sidebar.svelte does.
	$effect(() => {
		const tick = workspace.statusFocusTick;
		if (tick === 0) {
			return;
		}
		queueMicrotask(() => themePicker?.focus());
	});

	// Click outside the popover closes it. The pip button itself is
	// inside `containerWrap`, so clicks on it are excluded from the
	// "outside" check — `togglePanel` handles open/close on the pip.
	$effect(() => {
		if (!container.panelOpen) {
			return;
		}
		const onPointerDown = (event: PointerEvent) => {
			if (containerWrap && containerWrap.contains(event.target as Node)) {
				return;
			}
			container.closePanel();
		};
		const onKey = (event: KeyboardEvent) => {
			if (event.key === 'Escape') {
				container.closePanel();
			}
		};
		window.addEventListener('pointerdown', onPointerDown);
		window.addEventListener('keydown', onKey);
		return () => {
			window.removeEventListener('pointerdown', onPointerDown);
			window.removeEventListener('keydown', onKey);
		};
	});
</script>

<div class="status" data-region="status">
	<div class="left">
		{#if workspace.activeFolder}
			<span class="item">{workspace.activeFolder.host}</span>
			<span class="item path" title={workspace.activeFolder.path}>
				{workspace.activeFolder.path}
			</span>
		{/if}
	</div>
	<div class="right">
		{#if workspace.activeFile}
			<span class="item">
				{workspace.activeFile.name}{workspace.activeFile.isDirty ? ' •' : ''}
			</span>
		{/if}
		<!-- Diagnostic pill. Only renders when there's something to
			 say — zero-error / zero-warning buffers stay quiet. -->
		{#if errorCount > 0 || warnCount > 0}
			<span
				class="diag"
				title="{errorCount} error{errorCount === 1 ? '' : 's'}, {warnCount} warning{warnCount === 1 ? '' : 's'}"
			>
				{#if errorCount > 0}
					<span class="diag-chip error">
						<span class="diag-dot error" aria-hidden="true"></span>
						{errorCount}
					</span>
				{/if}
				{#if warnCount > 0}
					<span class="diag-chip warn">
						<span class="diag-dot warn" aria-hidden="true"></span>
						{warnCount}
					</span>
				{/if}
			</span>
		{/if}
		<!-- LSP availability pill. Only visible when the active file
			 has a wired-up language server AND that server isn't in
			 the happy `running` state. Kept plain text (no icon) so
			 it doesn't compete visually with the diagnostic pill. -->
		{#if showLspStatus && activeLspStatus}
			<span class="lsp-status status-{activeLspStatus.status}" title={lspStatusTitle}>
				{lspStatusLabel}
			</span>
		{/if}
		<!-- Container status pip. Hidden until we have a status snapshot
			 (no flash of "absent" while we're still resolving the
			 active workspace at startup). Click toggles the
			 ContainerPanel popover anchored just above. -->
		{#if container.visible}
			<div class="container-wrap" bind:this={containerWrap}>
				<button
					type="button"
					class="container"
					class:active={container.panelOpen}
					title="Container: {containerStateLabel(effectiveState)}"
					onclick={() => container.togglePanel()}
				>
					<span class="pip pip-{effectiveState}"></span>
					container
				</button>
				{#if container.panelOpen}
					<ContainerPanel />
				{/if}
			</div>
		{/if}
		<!-- Terminal launcher. Same popover the bottom-panel
			 strip uses; placed here so the user can spawn a
			 shell without opening the panel first. -->
		<TerminalLauncher anchor="above" variant="compact" title="Open terminal" />
		<!-- Chat panel toggle. Pip indicator shows connection state so
			 the user can see "Slack: connected" without opening the
			 panel. Independent dispatch from the command palette. -->
		<button
			type="button"
			class="chat"
			class:active={slack.panelVisible}
			title={slack.connected ? 'Chat (connected)' : 'Chat (not connected)'}
			onclick={() => slack.togglePanel()}
		>
			<span class="pip" class:on={slack.connected}></span>
			chat
		</button>
		<!-- Coder panel toggle. Same shape as the chat pip; pip lights
			 once the user has signed in to Hugging Face. The label
			 stays "coder" to mirror the panel header. -->
		<button
			type="button"
			class="chat"
			class:active={coder.panelVisible}
			title={coder.signedIn ? 'Coder (signed in)' : 'Coder (signed out)'}
			onclick={() => coder.togglePanel()}
		>
			<span class="pip" class:on={coder.signedIn}></span>
			coder
		</button>
		<!-- Theme picker popover. Three options: System (OS-driven),
			 Light, Dark. The trigger label reflects the stored choice
			 and its `title` tooltip also exposes the currently-
			 resolved mode when `System` is active — a useful
			 diagnostic if the IDE's colours diverge from the user's
			 expectation. Independent dispatch path from the command
			 palette, so a broken palette doesn't hide theme state. -->
		<ThemePicker bind:this={themePicker} />
	</div>
</div>

<style>
	.status {
		position: fixed;
		bottom: 0;
		left: 0;
		right: 0;
		height: 24px;
		background: var(--m-bg-1);
		border-top: 1px solid var(--m-border);
		display: flex;
		align-items: center;
		justify-content: space-between;
		padding: 0 8px;
		font-size: 11px;
		color: var(--m-fg-muted);
		z-index: 10;
	}
	.left,
	.right {
		display: flex;
		align-items: center;
		gap: 12px;
		min-width: 0;
	}
	.item {
		white-space: nowrap;
		text-overflow: ellipsis;
		overflow: hidden;
	}
	.path {
		max-width: 60ch;
		color: var(--m-fg-subtle);
	}
	.chat {
		font: inherit;
		color: var(--m-fg-muted);
		background: transparent;
		border: 1px solid transparent;
		border-radius: 4px;
		padding: 0 6px;
		height: 18px;
		line-height: 18px;
		display: flex;
		align-items: center;
		gap: 5px;
		cursor: pointer;
	}
	.chat:hover {
		background: var(--m-bg-overlay);
		color: var(--m-fg);
	}
	.chat.active {
		color: var(--m-fg);
	}
	.container-wrap {
		position: relative;
		display: flex;
		align-items: center;
	}
	.container {
		font: inherit;
		color: var(--m-fg-muted);
		background: transparent;
		border: 1px solid transparent;
		border-radius: 4px;
		padding: 0 6px;
		height: 18px;
		line-height: 18px;
		display: flex;
		align-items: center;
		gap: 5px;
		cursor: pointer;
	}
	.container:hover {
		background: var(--m-bg-overlay);
		color: var(--m-fg);
	}
	.container.active {
		color: var(--m-fg);
		background: var(--m-bg-overlay);
	}
	.pip {
		display: inline-block;
		width: 6px;
		height: 6px;
		border-radius: 50%;
		background: var(--m-fg-subtle);
	}
	.pip.on {
		background: var(--m-success);
	}
	/* Container pip colour-codes the high-level state. Same palette
	   as the ContainerPanel header so the two read as one signal. */
	.pip-absent {
		background: var(--m-fg-subtle);
	}
	.pip-creating {
		background: var(--m-warning, #d4a017);
		animation: pulse 1.6s ease-in-out infinite;
	}
	.pip-running {
		background: var(--m-success);
	}
	.pip-paused {
		background: var(--m-fg-muted);
		box-shadow: inset 0 0 0 1px var(--m-fg-subtle);
	}
	.pip-stopped {
		background: var(--m-fg-subtle);
		box-shadow: inset 0 0 0 1px var(--m-fg-muted);
	}
	.pip-failed {
		background: var(--m-danger);
	}
	@keyframes pulse {
		0%,
		100% {
			opacity: 1;
		}
		50% {
			opacity: 0.4;
		}
	}
	/* Diagnostic pill. Two chips side by side when both present;
	   severity colour drives the dot, text stays on `--m-fg` so
	   the count is legible regardless of background. */
	.diag {
		display: inline-flex;
		align-items: center;
		gap: 6px;
	}
	.diag-chip {
		display: inline-flex;
		align-items: center;
		gap: 4px;
		color: var(--m-fg);
	}
	.diag-dot {
		display: inline-block;
		width: 6px;
		height: 6px;
		border-radius: 50%;
	}
	.diag-dot.error {
		background: var(--m-danger);
	}
	.diag-dot.warn {
		background: var(--m-warning);
	}
	/* LSP availability pill. Subdued colour by default (not an
	   error state), amber during the transient `starting` window
	   so the user knows something's happening, red for crashed. */
	.lsp-status {
		font-size: 11px;
		color: var(--m-fg-muted);
		background: var(--m-bg-overlay);
		border-radius: 4px;
		padding: 0 6px;
		line-height: 18px;
		height: 18px;
		display: inline-flex;
		align-items: center;
		cursor: help;
	}
	.lsp-status.status-starting {
		color: var(--m-warning);
	}
	.lsp-status.status-crashed {
		color: var(--m-danger);
	}
</style>
