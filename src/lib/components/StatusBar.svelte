<script lang="ts">
	import { openUrl } from '@tauri-apps/plugin-opener';
	import { workspace } from '../state.svelte';
	import { slack } from '../slack.svelte';
	import { coder } from '../coder.svelte';
	import { container, containerStateLabel } from '../container.svelte';
	import { companion } from '../companion.svelte';
	import ContainerPanel from './ContainerPanel.svelte';
	import TerminalLauncher from './TerminalLauncher.svelte';
	import ThemePicker from './ThemePicker.svelte';
	import { lspLanguageFor } from '../editor/lspLanguage';
	import { bottomPanel } from '../bottomPanel.svelte';
	import { ports } from '../ports.svelte';

	let themePicker: ThemePicker | undefined = $state();
	let containerWrap: HTMLDivElement | undefined = $state();
	let autocompleteWrap: HTMLDivElement | undefined = $state();
	let autocompletePanelOpen = $state(false);
	let autocompleteExternalDraft = $state(workspace.nextEditExternalBaseUrl);
	let autocompleteBinaryDraft = $state(workspace.nextEditLlamaBinary);
	let autocompleteHfDraft = $state(workspace.nextEditHfRepo);
	let autocompleteHostDraft = $state(workspace.nextEditServerHost);
	let autocompletePortDraft = $state(String(workspace.nextEditServerPort));

	const SWEEP_NEXT_EDIT_BLOG = 'https://blog.sweep.dev/posts/oss-next-edit';

	const portsCount = $derived(ports.status.length);
	const portsAllLive = $derived(portsCount > 0 && ports.status.every((s) => s.health === 'live'));
	const portsBadgeTitle = $derived(
		portsCount === 0
			? 'No port forwards declared'
			: portsAllLive
				? `${portsCount} forward${portsCount === 1 ? '' : 's'} live`
				: `${portsCount} forward${portsCount === 1 ? '' : 's'} declared`,
	);

	function togglePortsPanel(): void {
		const existing = bottomPanel.findPortsTab();
		if (existing) {
			if (bottomPanel.activeId === existing.id && bottomPanel.visible) {
				bottomPanel.hide();
				return;
			}
			bottomPanel.setActive(existing.id);
			bottomPanel.show();
			return;
		}
		bottomPanel.addTab({ id: 'ports', title: 'Ports', kind: 'ports' });
		bottomPanel.show();
	}

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

	// Character count of the current editor selection, surfaced as
	// a tiny chip so the user can size up a buffer against an LLM
	// context budget (Ctrl+A on AGENTS.md → "12,345 selected"). We
	// use `text.length` — JS's UTF-16 code-unit count — on purpose:
	// for ASCII-dominated source / markdown it equals the visible
	// character count exactly, and even for emoji-heavy buffers it
	// stays a useful order-of-magnitude proxy without paying the
	// cost of an `Array.from(text)` walk on a multi-MB select-all.
	// The snapshot is sticky across tab switches (see
	// `state.svelte.ts` activeSelection docs — the "Add to Coder"
	// flow wants it), so we gate the chip on the snapshot's path
	// matching the active file. Otherwise switching to a fresh
	// buffer would still show the previous file's selection size.
	const selectionChars = $derived.by(() => {
		const sel = workspace.activeSelection;
		if (sel === null || workspace.activeFile === null) {
			return 0;
		}
		if (sel.path !== workspace.activeFile.path) {
			return 0;
		}
		return sel.text.length;
	});

	// Per-language LSP pill. Visible whenever the active file maps
	// to a language we have a server wired up for (so a Markdown
	// buffer doesn't render "typescript: ..." against a server that
	// has nothing to do with the open file). We show the pill in
	// every state — including the steady-state `running` — so the
	// user always has a one-click handle into that server's logs;
	// the colour and label tell the steady-state apart from the
	// noisy ones at a glance.
	//
	// `activeLspStatus` may be momentarily `null` between "the user
	// just opened the first file of this language" and "the broker
	// emitted its first `lsp:status` event": LSP servers spawn
	// lazily on the first `lsp_open` IPC, so the in-memory
	// `lspStatuses` map is empty until then. We render a placeholder
	// pill in that window — same visual treatment as `starting` —
	// so the affordance doesn't flicker in for a frame and out
	// again.
	//
	// Statuses we surface:
	//   - `running` — happy path, subdued colour, just shows the
	//     language id; clicking opens the logs (rare; mostly for
	//     debugging) with no hint suffix in the tooltip
	//   - `starting` — ~3-5s spinner while the server boots
	//   - `notavailable` — nudge the user to install the binary
	//   - `crashed` — last run died; next file open will retry
	//   - `stopped` — shutdown fired (during workspace close)
	const activeLanguage = $derived(workspace.activeFile ? lspLanguageFor(workspace.activeFile.path) : null);
	const activeLspStatus = $derived(activeLanguage ? (workspace.lspStatuses.get(activeLanguage) ?? null) : null);
	const showLspStatus = $derived(activeLanguage !== null);
	const lspDisplayLanguageId = $derived(activeLspStatus?.languageId ?? activeLanguage ?? '');
	const lspStatusKind = $derived(activeLspStatus?.status ?? 'pending');
	const lspStatusLabel = $derived.by(() => {
		const s = activeLspStatus;
		if (!s) {
			return lspDisplayLanguageId ? `${lspDisplayLanguageId}: starting…` : '';
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
			return lspDisplayLanguageId ? `${lspDisplayLanguageId} language server: starting — click to open logs` : '';
		}
		// "Click to open logs" hint on every state now that the
		// pill is always clickable — the tooltip is the only
		// place the affordance is discoverable from.
		if (s.status === 'notavailable') {
			const base = s.detail ?? `Install a language server for ${s.languageId}`;
			return `${base} — click to open logs`;
		}
		if (s.status === 'crashed') {
			const base = s.detail ?? `${s.languageId} language server crashed`;
			return `${base} — click to open logs`;
		}
		const base = s.detail ?? lspStatusLabel;
		return `${base} — click to open logs`;
	});

	/**
	 * Open (or focus) the diagnostic-logs tab for `languageId`.
	 * Mirrors the `pick(source)` flow in [`LogsLauncher`] —
	 * matched diag tabs share `source: 'lsp.<languageId>'` with
	 * the broker convention in `moon_core::lsp::broker::log_source_for`.
	 * Used by the status-bar pill click handler so a user who
	 * sees any state — running, starting, crashed, … — lands
	 * directly on that server's stderr without having to open
	 * the picker.
	 */
	function openLspLogPanel(languageId: string): void {
		const source = `lsp.${languageId}`;
		const existing = bottomPanel.findDiagTab(source);
		if (existing) {
			bottomPanel.setActive(existing.id);
		} else {
			bottomPanel.addTab({
				id: `diag:${source}`,
				title: source,
				kind: 'diag',
				source,
			});
		}
		bottomPanel.show();
	}

	// Optimistic state during the two long-running ops (setup,
	// rebuild) so the pip transitions immediately rather than
	// staying on the previous glyph for a few minutes while
	// `up -d --wait` is in flight. Pause / resume / teardown are
	// quick enough that flicker isn't worth the extra branching.
	const effectiveState = $derived(
		container.inFlight === 'setup' || container.inFlight === 'rebuild' ? 'creating' : container.state,
	);

	const autocompletePipClass = $derived.by(() => {
		if (workspace.autocompleteInFlight) {
			return 'pip-loading';
		}
		const k = workspace.nextEditProbe?.kind;
		if (k === 'ready') {
			return 'on';
		}
		if (k === 'model_loading') {
			return 'pip-loading';
		}
		if (k === 'unreachable' || k === 'error') {
			return 'pip-warn';
		}
		return '';
	});

	const autocompleteBarTitle = $derived.by(() => {
		if (workspace.autocompleteInFlight) {
			return 'Autocomplete — model running…';
		}
		const p = workspace.nextEditProbe;
		if (workspace.nextEditProbeInFlight && p === null) {
			return 'Autocomplete — checking…';
		}
		if (p?.kind === 'ready') {
			return 'Autocomplete — ready';
		}
		if (p?.kind === 'model_loading') {
			return 'Autocomplete — loading model';
		}
		if (p?.kind === 'unreachable' || p?.kind === 'error') {
			return 'Autocomplete — not connected';
		}
		return 'Autocomplete — click to set up';
	});

	const autocompleteShortcutVisible = $derived.by(() => {
		if (workspace.autocompleteInFlight) {
			return true;
		}
		if (workspace.nextEditServerSnapshot?.running) {
			return true;
		}
		const k = workspace.nextEditProbe?.kind;
		return k === 'ready' || k === 'model_loading';
	});

	const autocompleteStatusText = $derived.by(() => {
		if (workspace.autocompleteInFlight) {
			return 'Calling the autocomplete model…';
		}
		if (workspace.nextEditProbeInFlight && workspace.nextEditProbe === null) {
			return 'Checking…';
		}
		const p = workspace.nextEditProbe;
		if (!p) {
			return 'Status unknown — when ready, Ctrl+T applies an autocomplete patch (Ctrl+Space is LSP only).';
		}
		switch (p.kind) {
			case 'ready':
				return 'Ready — Ctrl+T patches the buffer from the local model. Ctrl+Space is LSP completion only.';
			case 'unreachable':
				return 'Not connected. Press Start, or use Advanced if the server runs elsewhere.';
			case 'model_loading':
				return 'Still loading — first launch can take a few minutes.';
			case 'error':
				return p.detail ?? 'Something went wrong.';
			default:
				return '';
		}
	});

	function toggleAutocompletePanel() {
		autocompletePanelOpen = !autocompletePanelOpen;
		if (autocompletePanelOpen) {
			autocompleteExternalDraft = workspace.nextEditExternalBaseUrl;
			autocompleteBinaryDraft = workspace.nextEditLlamaBinary;
			autocompleteHfDraft = workspace.nextEditHfRepo;
			autocompleteHostDraft = workspace.nextEditServerHost;
			autocompletePortDraft = String(workspace.nextEditServerPort);
			void workspace.refreshNextEditServerStatus();
			void workspace.refreshNextEditProbe();
		}
	}

	function closeAutocompletePanel() {
		autocompletePanelOpen = false;
	}

	function applyAutocompleteExternal() {
		workspace.setNextEditExternalBaseUrl(autocompleteExternalDraft);
		autocompleteExternalDraft = workspace.nextEditExternalBaseUrl;
	}

	function applyAutocompleteListen() {
		const port = Number.parseInt(autocompletePortDraft, 10);
		if (!Number.isFinite(port) || port < 1 || port > 65535) {
			workspace.flash('Port must be between 1 and 65535.');
			return;
		}
		workspace.setNextEditServerHost(autocompleteHostDraft);
		workspace.setNextEditServerPort(port);
	}

	function applyAutocompleteBinary() {
		workspace.setNextEditLlamaBinary(autocompleteBinaryDraft);
	}

	function applyAutocompleteHf() {
		workspace.setNextEditHfRepo(autocompleteHfDraft);
	}

	function saveAutocompleteDrafts() {
		applyAutocompleteListen();
		applyAutocompleteBinary();
		applyAutocompleteHf();
	}

	$effect(() => {
		if (!autocompletePanelOpen) {
			return;
		}
		const id = window.setInterval(() => {
			void workspace.refreshNextEditServerStatus();
			void workspace.refreshNextEditProbe();
		}, 1000);
		return () => {
			window.clearInterval(id);
		};
	});

	// Keep a light poll of the companion bridge status alive while the
	// status bar is mounted, so the companion pip reflects running /
	// paired state at rest (cheap local file read every few seconds).
	$effect(() => {
		companion.startPolling();
		return () => {
			companion.stopPolling();
		};
	});

	$effect(() => {
		if (!autocompletePanelOpen) {
			return;
		}
		const onPointerDown = (event: PointerEvent) => {
			if (autocompleteWrap && autocompleteWrap.contains(event.target as Node)) {
				return;
			}
			closeAutocompletePanel();
		};
		const onKey = (event: KeyboardEvent) => {
			if (event.key === 'Escape') {
				closeAutocompletePanel();
			}
		};
		window.addEventListener('pointerdown', onPointerDown);
		window.addEventListener('keydown', onKey);
		return () => {
			window.removeEventListener('pointerdown', onPointerDown);
			window.removeEventListener('keydown', onKey);
		};
	});

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
		<!-- Selection size chip. Hidden when there's no selection
			 so the status bar stays quiet during regular editing.
			 Tooltip spells out the unit; the chip itself stays
			 compact ("12,345 sel") to leave room for the other
			 status pills. -->
		{#if selectionChars > 0}
			<span class="item sel" title="{selectionChars.toLocaleString()} characters selected">
				{selectionChars.toLocaleString()} sel
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
		<!-- LSP pill. Visible whenever the active file has a wired-
			 up language server, regardless of the server's state —
			 the colour distinguishes the noisy states from the
			 happy path, and clicking always opens the LSP log
			 panel for that language. Kept plain text (no icon) so
			 it doesn't compete visually with the diagnostic pill
			 to its left. -->
		{#if showLspStatus && lspDisplayLanguageId !== ''}
			<button
				type="button"
				class="lsp-status status-{lspStatusKind} clickable"
				title={lspStatusTitle}
				onclick={() => openLspLogPanel(lspDisplayLanguageId)}
			>
				{lspStatusLabel}
			</button>
		{/if}
		<!-- Container status pip. Hidden until we have a status snapshot
			 (no flash of "absent" while we're still resolving the
			 active workspace at startup). Click toggles the
			 ContainerPanel popover anchored just above. -->
		<div class="autocomplete-wrap" bind:this={autocompleteWrap}>
			<button
				type="button"
				class="chat"
				class:active={autocompletePanelOpen}
				title={autocompleteBarTitle}
				onclick={() => toggleAutocompletePanel()}
			>
				<span
					class="pip"
					class:on={autocompletePipClass === 'on'}
					class:pip-warn={autocompletePipClass === 'pip-warn'}
					class:pip-loading={autocompletePipClass === 'pip-loading'}
				></span>
				<span class="autocomplete-label">autocomplete</span>
				{#if autocompleteShortcutVisible}
					<kbd class="autocomplete-kbd">Ctrl+T</kbd>
				{/if}
			</button>
			{#if autocompletePanelOpen}
				<div class="autocomplete-panel" role="dialog" aria-label="Autocomplete">
					<header class="ne-head">
						<span class="ne-title">Autocomplete</span>
					</header>
					<p class="ne-status">{autocompleteStatusText}</p>
					{#if workspace.nextEditExternalBaseUrl.trim().length > 0}
						<p class="ne-hint ne-banner">Using your own server (Advanced). Clear that URL to use Start below.</p>
					{/if}
					<div class="ne-actions" role="toolbar" aria-label="Model server">
						<button
							type="button"
							class="ne-apply"
							disabled={workspace.nextEditServerActionInFlight ||
								workspace.nextEditExternalBaseUrl.trim().length > 0 ||
								(!autocompleteHfDraft.trim() && !workspace.nextEditHfRepo.trim()) ||
								workspace.nextEditServerSnapshot?.running}
							onclick={() => {
								saveAutocompleteDrafts();
								void workspace.startNextEditServer();
							}}
						>
							Start
						</button>
						<button
							type="button"
							class="ne-apply"
							disabled={workspace.nextEditServerActionInFlight || !workspace.nextEditServerSnapshot?.running}
							onclick={() => void workspace.stopNextEditServer()}
						>
							Stop
						</button>
					</div>
					<label class="ne-label" for="ne-bin">Server command</label>
					<input
						id="ne-bin"
						class="ne-input ne-input-block"
						type="text"
						placeholder="llama-server"
						bind:value={autocompleteBinaryDraft}
					/>
					<label class="ne-label" for="ne-hf">Model</label>
					<input
						id="ne-hf"
						class="ne-input ne-input-block"
						type="text"
						placeholder="Hugging Face repo"
						bind:value={autocompleteHfDraft}
					/>
					<label class="ne-label" for="ne-host">Listen address</label>
					<div class="ne-url-row ne-listen-row">
						<input id="ne-host" class="ne-input ne-host" type="text" bind:value={autocompleteHostDraft} />
						<input
							class="ne-input ne-port"
							type="text"
							inputmode="numeric"
							aria-label="Port"
							bind:value={autocompletePortDraft}
						/>
					</div>
					<button type="button" class="ne-apply ne-save-all" onclick={() => saveAutocompleteDrafts()}>
						Save settings
					</button>
					<details class="ne-advanced">
						<summary>Advanced</summary>
						<p class="ne-hint">Only if you start the server yourself — paste its URL here.</p>
						<div class="ne-url-row">
							<input
								class="ne-input"
								type="text"
								placeholder="http://127.0.0.1:8080"
								bind:value={autocompleteExternalDraft}
							/>
							<button type="button" class="ne-apply" onclick={() => applyAutocompleteExternal()}>Save</button>
						</div>
					</details>
					{#if workspace.nextEditServerSnapshot && workspace.nextEditServerSnapshot.logTail.length > 0}
						<details class="ne-logs">
							<summary>Server log (recent)</summary>
							<pre class="ne-log-pre">{workspace.nextEditServerSnapshot.logTail.join('\n')}</pre>
						</details>
					{/if}
					<p class="ne-hint">
						Needs a local
						<code>llama-server</code>
						(
						<button type="button" class="ne-link" onclick={() => void openUrl('https://github.com/ggml-org/llama.cpp')}>
							llama.cpp
						</button>
						). First run downloads the model; closing the app stops the server.
						<button type="button" class="ne-link" onclick={() => void openUrl(SWEEP_NEXT_EDIT_BLOG)}>
							Format notes
						</button>
					</p>
				</div>
			{/if}
		</div>
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
			<button type="button" class="container" title={portsBadgeTitle} onclick={togglePortsPanel}>
				<span class="pip" class:on={portsAllLive}></span>
				ports{portsCount > 0 ? ` (${portsCount})` : ''}
			</button>
		{/if}
		<!-- Companion (mobile pairing). Pip reflects bridge running
			 state; click opens the pairing modal (QR + devices). The
			 same modal is reachable from the command palette. -->
		<button
			type="button"
			class="container"
			class:active={companion.modalOpen}
			title={companion.remoteConnected
				? `Connected to relay ${companion.remoteStatus?.bridge_url ?? ''}${companion.connectedPhoneCount > 0 ? ` · ${companion.connectedPhoneCount} phone${companion.connectedPhoneCount === 1 ? '' : 's'} connected` : ''}`
				: companion.remoteErrored
					? `Relay connection failing: ${companion.remoteStatus?.error ?? ''}`
					: companion.running
						? `Companion bridge running${companion.deviceCount > 0 ? ` · ${companion.deviceCount} device${companion.deviceCount === 1 ? '' : 's'} paired` : ' · nothing paired yet'}`
						: 'Companion bridge not running'}
			onclick={() => companion.toggle()}
		>
			<span class="icon" aria-hidden="true">☏</span>
			<span class="pip" class:on={companion.active} class:pip-warn={companion.remoteErrored}></span>
			companion{companion.connectedPhoneCount > 0 ? ` (${companion.connectedPhoneCount})` : ''}
		</button>
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
	.sel {
		color: var(--m-fg-muted);
		font-variant-numeric: tabular-nums;
		cursor: help;
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
	.container .icon {
		font-size: 12px;
		line-height: 1;
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
	.pip.pip-warn {
		background: var(--m-warning, #d4a017);
	}
	.pip.pip-loading {
		background: var(--m-warning, #d4a017);
		animation: pulse 1.6s ease-in-out infinite;
	}
	.autocomplete-wrap {
		position: relative;
		display: flex;
		align-items: center;
	}
	.autocomplete-label {
		flex-shrink: 0;
	}
	.autocomplete-kbd {
		font: inherit;
		font-family: var(--m-font-mono, monospace);
		font-size: 9px;
		padding: 0 4px;
		border-radius: 3px;
		border: 1px solid var(--m-border);
		background: var(--m-bg-overlay);
		color: var(--m-fg-subtle);
		line-height: 14px;
	}
	.autocomplete-panel {
		position: absolute;
		bottom: 100%;
		right: 0;
		margin-bottom: 6px;
		min-width: 300px;
		max-width: 440px;
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
	.ne-head {
		display: flex;
		align-items: baseline;
		justify-content: space-between;
		gap: 8px;
	}
	.ne-title {
		font-weight: 600;
		text-transform: lowercase;
	}
	.ne-sub {
		color: var(--m-fg-muted);
		font-variant-numeric: tabular-nums;
	}
	.ne-status {
		margin: 0;
		color: var(--m-fg-muted);
		line-height: 1.4;
	}
	.ne-row {
		display: flex;
		align-items: center;
		gap: 8px;
		cursor: pointer;
		color: var(--m-fg);
	}
	.ne-label {
		font-size: 11px;
		color: var(--m-fg-muted);
		margin: 0;
	}
	.ne-url-row {
		display: flex;
		gap: 6px;
	}
	.ne-input {
		flex: 1 1 auto;
		min-width: 0;
		font: inherit;
		font-size: 12px;
		padding: 4px 6px;
		border-radius: 4px;
		border: 1px solid var(--m-border);
		background: var(--m-bg-1);
		color: var(--m-fg);
	}
	.ne-input-block {
		width: 100%;
		box-sizing: border-box;
	}
	.ne-save-all {
		width: 100%;
		margin-top: 2px;
	}
	.ne-apply {
		font: inherit;
		font-size: 12px;
		padding: 4px 10px;
		border-radius: 4px;
		border: 1px solid var(--m-border);
		background: var(--m-bg-overlay);
		color: var(--m-fg);
		cursor: pointer;
		flex-shrink: 0;
	}
	.ne-apply:hover {
		border-color: var(--m-border-strong);
	}
	.ne-apply:disabled {
		opacity: 0.45;
		cursor: not-allowed;
	}
	.ne-actions {
		display: flex;
		gap: 6px;
		flex-wrap: wrap;
	}
	.ne-listen-row {
		align-items: stretch;
	}
	.ne-host {
		flex: 2 1 120px;
	}
	.ne-port {
		flex: 0 0 4.5rem;
		max-width: 5rem;
	}
	.ne-logs {
		margin: 0;
		color: var(--m-fg-muted);
		font-size: 11px;
	}
	.ne-logs summary {
		cursor: pointer;
		user-select: none;
	}
	.ne-log-pre {
		margin: 6px 0 0;
		padding: 8px;
		max-height: 160px;
		overflow: auto;
		border-radius: 4px;
		background: var(--m-bg-overlay);
		border: 1px solid var(--m-border);
		font-size: 10px;
		line-height: 1.35;
		white-space: pre-wrap;
		word-break: break-word;
		color: var(--m-fg);
	}
	.ne-hint {
		margin: 0;
		color: var(--m-fg-muted);
		line-height: 1.4;
		font-size: 11px;
	}
	.ne-hint code {
		font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
		background: var(--m-bg-overlay);
		padding: 0 4px;
		border-radius: 3px;
		color: var(--m-fg);
	}
	.ne-banner {
		padding: 6px 8px;
		border-radius: 4px;
		border: 1px solid var(--m-border);
		background: var(--m-bg-overlay);
		color: var(--m-fg-muted);
	}
	.ne-advanced {
		margin: 0;
		padding: 6px 0 0;
		border-top: 1px solid var(--m-border);
		color: var(--m-fg-muted);
		font-size: 11px;
	}
	.ne-advanced summary {
		cursor: pointer;
		user-select: none;
		color: var(--m-fg);
		font-weight: 500;
	}
	.ne-code {
		margin: 0;
		padding: 8px;
		border-radius: 4px;
		background: var(--m-bg-overlay);
		border: 1px solid var(--m-border);
		font-size: 11px;
		line-height: 1.35;
		overflow-x: auto;
		white-space: pre-wrap;
		word-break: break-all;
	}
	.ne-link {
		display: inline;
		padding: 0;
		margin: 0;
		border: none;
		background: none;
		font: inherit;
		color: var(--m-accent, #6b9eff);
		cursor: pointer;
		text-decoration: underline;
		text-align: left;
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
	/* LSP pill. Always rendered as a `<button>` now; the selectors
	   below reset the default browser chrome and apply per-state
	   colour. Subdued for the steady-state `running` and
	   `notavailable` (informational, not actionable from the pill
	   itself), amber during the transient `starting` window and the
	   pre-event `pending` placeholder so the user knows something's
	   happening, red for crashed. `.clickable` is kept as an
	   explicit affordance class for parity with other status-bar
	   buttons; today every render has it set. */
	.lsp-status {
		font: inherit;
		font-size: 11px;
		color: var(--m-fg-muted);
		background: var(--m-bg-overlay);
		border: 1px solid transparent;
		border-radius: 4px;
		padding: 0 6px;
		line-height: 18px;
		height: 18px;
		display: inline-flex;
		align-items: center;
		cursor: pointer;
	}
	.lsp-status.status-pending,
	.lsp-status.status-starting {
		color: var(--m-warning);
	}
	.lsp-status.status-crashed {
		color: var(--m-danger);
	}
	.lsp-status:hover {
		border-color: var(--m-border-strong);
	}
	.lsp-status:focus-visible {
		outline: 1px solid var(--m-accent);
		outline-offset: 1px;
	}
</style>
