<script lang="ts">
	import { onMount, onDestroy } from 'svelte';
	import { writeText as clipboardWriteText } from '@tauri-apps/plugin-clipboard-manager';
	import { renderSVG } from 'uqr';
	import { ipc, type AuthorizedPhone, type PairingQr } from '../ipc';
	import { formatError } from '../protocol';
	import { companion } from '../companion.svelte';

	// The store owns the polled status (shared with the status-bar
	// pip); the modal just renders it and keeps a poll ref alive while
	// open so it refreshes a touch faster after a pair/revoke.
	const status = $derived(companion.status);

	// Tab: "local" = this machine runs the bridge (Phase 13 QR flow);
	// "remote" = the IDE dials out to a relay bridge elsewhere (14.3).
	// Defaults to local; auto-switches to remote if already connected.
	let tab = $state<'local' | 'remote'>('remote');

	// Phones this host authorized end-to-end (ADR 0087) — host-wide,
	// so both tabs show the same list.
	let phones = $state<AuthorizedPhone[]>([]);
	let phonesError = $state<string | null>(null);

	async function refreshPhones(): Promise<void> {
		try {
			phones = await ipc.companion.e2eDevices();
			phonesError = null;
		} catch (err) {
			phonesError = formatError(err);
		}
	}

	async function revokePhone(id: string): Promise<void> {
		await ipc.companion.e2eRevoke(id);
		await refreshPhones();
	}

	let copied = $state(false);

	async function copyLink(link: string): Promise<void> {
		// The Tauri plugin first: WebKitGTK rejects navigator.clipboard
		// from an unfocused button (same fallback as editorContextMenu).
		try {
			await clipboardWriteText(link);
		} catch {
			await navigator.clipboard.writeText(link);
		}
		copied = true;
		setTimeout(() => (copied = false), 1500);
	}

	onMount(() => {
		companion.startPolling();
		void refreshPhones();
		void companion.refresh();
		void companion.refreshRemote().then(() => {
			// Re-enrolling usually means the same relay.
			const current = companion.remoteStatus?.bridge_url;
			if (current) {
				enrollUrl = current;
			}
		});
	});

	onDestroy(() => {
		companion.stopPolling();
	});

	// Local-bridge pairing is on-demand (Phase 14.5): the button mints
	// a fresh single-use pairing link (ADR 0087). The QR and the copy
	// button carry the same link — no short typed code exists.
	let localPair = $state<PairingQr | null>(null);
	let localPairError = $state<string | null>(null);
	const qrSvg = $derived(localPair ? renderSVG(localPair.payload, { border: 2 }) : null);

	async function requestLocalPairCode(): Promise<void> {
		localPairError = null;
		try {
			localPair = await ipc.companion.pairCode();
		} catch (err) {
			localPair = null;
			localPairError = formatError(err);
		}
	}

	async function revoke(id: string): Promise<void> {
		// Revoke is synchronous over the control socket, so refresh
		// immediately — no poll delay needed.
		await ipc.companion.revokeDevice(id);
		await companion.refresh();
	}

	async function revokeIde(ideId: string): Promise<void> {
		await ipc.companion.revokeIde(ideId);
		await companion.refresh();
	}

	// Remote-bridge enroll form state (Phase 14.3). The command
	// resolves only once the handshake succeeds or fails, so the busy
	// flag covers the real outcome, not just the spawn.
	// The last relay this machine enrolled with, so a retry or a
	// re-enroll after Disconnect doesn't mean typing it again.
	const LAST_RELAY_KEY = 'moon-companion-last-relay';
	let enrollUrl = $state(localStorage.getItem(LAST_RELAY_KEY) ?? '');
	let enrollCode = $state('');
	let enrollError = $state<string | null>(null);
	let enrolling = $state(false);

	async function enrollRemote(event: Event): Promise<void> {
		event.preventDefault();
		enrollError = null;
		enrolling = true;
		localStorage.setItem(LAST_RELAY_KEY, enrollUrl.trim());
		try {
			await ipc.companion.enroll(enrollUrl, enrollCode, 'moon-ide');
			await companion.refreshRemote();
			enrollCode = '';
		} catch (err) {
			enrollError = formatError(err);
		} finally {
			enrolling = false;
		}
	}

	async function disconnectRemote(): Promise<void> {
		await ipc.companion.remoteDisconnect();
		await companion.refreshRemote();
	}

	// IDE-minted phone pairing via the remote bridge (Phase 14.5).
	// The bridge trusts enrolled IDEs to open pairing windows, so the
	// QR renders right here — no relay-box journal digging.
	let remotePair = $state<PairingQr | null>(null);
	let remotePairError = $state<string | null>(null);
	const remotePairQrSvg = $derived(remotePair ? renderSVG(remotePair.payload, { border: 2 }) : null);

	async function requestRemotePairCode(): Promise<void> {
		remotePairError = null;
		try {
			remotePair = await ipc.companion.remotePairCode();
		} catch (err) {
			remotePair = null;
			remotePairError = formatError(err);
		}
	}

	function hostOf(url: string): string {
		return url.replace(/^wss?:\/\//, '').replace(/\/$/, '');
	}

	function relativeTime(ms: number): string {
		const mins = Math.round((Date.now() - ms) / 60000);
		if (mins < 1) {
			return 'just now';
		}
		if (mins < 60) {
			return `${mins}m ago`;
		}
		const hours = Math.round(mins / 60);
		return hours < 24 ? `${hours}h ago` : `${Math.round(hours / 24)}d ago`;
	}
</script>

{#snippet pairLink(pair: PairingQr, svg: string | null)}
	{#if svg}
		<!-- eslint-disable-next-line svelte/no-at-html-tags -->
		<div class="qr">{@html svg}</div>
	{/if}
	<div class="link-row">
		<code class="link">{pair.payload}</code>
		<button type="button" class="secondary" onclick={() => copyLink(pair.payload)}>{copied ? 'Copied' : 'Copy'}</button>
	</div>
	<p class="hint">
		Single use, valid 15 minutes. It carries this IDE's key: share it only with the phone you're pairing.
	</p>
{/snippet}

{#snippet error(message: string)}
	<p class="error">{message}</p>
{/snippet}

{#snippet deviceList(rows: { id: string; label: string; at: number }[], onRevoke: (id: string) => void, empty: string)}
	{#if rows.length === 0}
		<p class="hint">{empty}</p>
	{:else}
		<ul class="devices">
			{#each rows as row (row.id)}
				<li>
					<span class="device-label">{row.label}</span>
					<span class="device-meta">{relativeTime(row.at)}</span>
					<button type="button" class="link-button danger" onclick={() => onRevoke(row.id)}>Revoke</button>
				</li>
			{/each}
		</ul>
	{/if}
{/snippet}

{#snippet allowedPhones()}
	<section>
		<h3>Paired phones</h3>
		{#if phonesError}
			{@render error(phonesError)}
		{:else}
			{@render deviceList(
				phones.map((p) => ({ id: p.id, label: p.label, at: p.paired_at_ms })),
				(id) => void revokePhone(id),
				'None yet.',
			)}
		{/if}
	</section>
{/snippet}

<div class="overlay" role="dialog" aria-modal="true" aria-label="Companion">
	<div class="card">
		<header>
			<h2>Companion</h2>
			<div class="tabs" role="tablist">
				<button
					type="button"
					role="tab"
					aria-selected={tab === 'remote'}
					class:active={tab === 'remote'}
					onclick={() => {
						tab = 'remote';
						void companion.refreshRemote();
					}}>Remote relay</button
				>
				<button
					type="button"
					role="tab"
					aria-selected={tab === 'local'}
					class:active={tab === 'local'}
					onclick={() => (tab = 'local')}>Local network</button
				>
			</div>
			<button type="button" class="close" aria-label="Close" onclick={() => companion.close()}>×</button>
		</header>

		{#if tab === 'remote'}
			{@const remote = companion.remoteStatus}
			<div class="status" class:ok={remote?.connected} class:bad={!remote?.connected && remote?.error}>
				<span class="dot" aria-hidden="true"></span>
				{#if remote?.connected}
					<span>Connected to <strong>{hostOf(remote.bridge_url)}</strong> as <strong>{remote.ide_id}</strong></span>
				{:else if remote?.error}
					<span>Not connected to the relay</span>
				{:else}
					<span>Not connected to a relay</span>
				{/if}
			</div>

			{#if remote?.connected}
				<section>
					<h3>Pair a phone</h3>
					{#if remotePair}
						{@render pairLink(remotePair, remotePairQrSvg)}
					{/if}
					<div class="actions">
						<button type="button" class="primary" onclick={() => requestRemotePairCode()}>
							{remotePair ? 'New pairing link' : 'Show pairing QR'}
						</button>
					</div>
					{#if remotePairError}
						{@render error(remotePairError)}
					{/if}
				</section>
				{@render allowedPhones()}
				<footer>
					<p class="hint">Calls are end-to-end encrypted; the relay only routes them.</p>
					<button type="button" class="link-button danger" onclick={() => disconnectRemote()}>Disconnect</button>
				</footer>
			{:else}
				{#if remote?.error}
					{@render error(remote.error)}
				{/if}
				<form onsubmit={enrollRemote} class="enroll">
					<label>
						<span class="field-label">Relay</span>
						<input
							type="text"
							placeholder="wss://bridge.example.com"
							bind:value={enrollUrl}
							required
							disabled={enrolling}
							spellcheck="false"
						/>
					</label>
					<label>
						<span class="field-label">Enrollment code</span>
						<input
							type="text"
							placeholder="XXXX-XXXX"
							bind:value={enrollCode}
							required
							disabled={enrolling}
							spellcheck="false"
						/>
					</label>
					<p class="hint">
						Run <code>moon-bridge enroll-code</code> where the relay runs for a code (valid 120 s). Enrollment survives relay
						restarts, so this is once per machine.
					</p>
					<div class="actions">
						<button type="submit" class="primary" disabled={enrolling}>{enrolling ? 'Enrolling…' : 'Enroll'}</button>
					</div>
				</form>
				{#if enrollError}
					{@render error(enrollError)}
				{/if}
			{/if}
		{:else if !status || !status.running}
			<div class="status"><span class="dot" aria-hidden="true"></span><span>The local bridge isn't running</span></div>
			<p class="hint">
				It starts with a release build of moon-ide. In a dev session, run
				<code>moon-bridge serve --web-root companion/dist</code>, then reopen this dialog.
			</p>
		{:else}
			<div class="status ok">
				<span class="dot" aria-hidden="true"></span>
				<span>Listening on <strong>{status.mdns_url ?? status.url}</strong></span>
			</div>
			<section>
				<h3>Pair a phone on this network</h3>
				{#if localPair}
					{@render pairLink(localPair, qrSvg)}
				{/if}
				<div class="actions">
					<button type="button" class="primary" onclick={() => requestLocalPairCode()}>
						{localPair ? 'New pairing link' : 'Show pairing QR'}
					</button>
				</div>
				{#if localPairError}
					{@render error(localPairError)}
				{/if}
				<p class="hint">
					First time on a device, accept the self-signed certificate. Fingerprint
					<code class="fingerprint">{status.fingerprint}</code>
				</p>
			</section>
			{@render allowedPhones()}
			<section>
				<h3>Relay routing tokens</h3>
				{@render deviceList(
					status.devices.map((d) => ({ id: d.id, label: d.label, at: d.paired_at_ms })),
					(id) => void revoke(id),
					'None yet.',
				)}
			</section>
			{#if status.ides && status.ides.length > 0}
				<section>
					<h3>IDEs enrolled with this bridge</h3>
					{@render deviceList(
						status.ides.map((i) => ({ id: i.id, label: i.label, at: i.enrolled_at_ms })),
						(id) => void revokeIde(id),
						'None.',
					)}
				</section>
			{/if}
		{/if}
	</div>
</div>

<style>
	.overlay {
		position: fixed;
		inset: 0;
		background: rgba(0, 0, 0, 0.55);
		display: flex;
		align-items: center;
		justify-content: center;
		z-index: 1000;
	}
	.card {
		background: var(--m-bg-1);
		color: var(--m-fg);
		border: 1px solid var(--m-border);
		border-radius: var(--m-radius-lg);
		box-shadow: 0 12px 36px rgba(0, 0, 0, 0.55);
		padding: 16px 20px 18px;
		width: min(460px, calc(100% - 32px));
		max-height: 88vh;
		overflow-y: auto;
		display: flex;
		flex-direction: column;
		gap: 14px;
		font-size: var(--m-text-md);
	}
	header {
		display: flex;
		align-items: center;
		gap: 12px;
	}
	h2 {
		margin: 0;
		font-size: var(--m-text-lg);
		font-weight: 600;
	}
	h3 {
		margin: 0 0 6px;
		font-size: var(--m-text-xs);
		font-weight: 600;
		letter-spacing: 0.06em;
		text-transform: uppercase;
		color: var(--m-fg-subtle);
	}
	section {
		display: flex;
		flex-direction: column;
		gap: 8px;
	}
	.tabs {
		display: flex;
		margin-left: auto;
		border: 1px solid var(--m-border);
		border-radius: var(--m-radius-md);
		overflow: hidden;
	}
	.tabs button {
		background: none;
		border: none;
		color: var(--m-fg-muted);
		font: inherit;
		font-size: var(--m-text-sm);
		padding: 4px 10px;
		cursor: pointer;
	}
	.tabs button + button {
		border-left: 1px solid var(--m-border);
	}
	.tabs button.active {
		background: var(--m-bg-3);
		color: var(--m-fg);
	}
	.close {
		background: none;
		border: none;
		color: var(--m-fg-muted);
		font-size: 18px;
		line-height: 1;
		padding: 0 2px;
		cursor: pointer;
	}
	.close:hover {
		color: var(--m-fg);
	}
	.status {
		display: flex;
		align-items: center;
		gap: 8px;
		padding: 8px 10px;
		border-radius: var(--m-radius-md);
		background: var(--m-bg-2);
		color: var(--m-fg-muted);
	}
	.status strong {
		color: var(--m-fg);
		font-weight: 600;
	}
	.dot {
		flex: none;
		width: 8px;
		height: 8px;
		border-radius: 50%;
		background: var(--m-fg-subtle);
	}
	.status.ok .dot {
		background: var(--m-success);
	}
	.status.bad .dot {
		background: var(--m-danger);
	}
	.hint {
		margin: 0;
		color: var(--m-fg-muted);
		font-size: var(--m-text-sm);
		line-height: 1.5;
	}
	.error {
		margin: 0;
		color: var(--m-danger);
		font-size: var(--m-text-sm);
		line-height: 1.5;
	}
	.actions {
		display: flex;
		gap: 8px;
	}
	button.primary,
	button.secondary {
		font: inherit;
		font-size: var(--m-text-sm);
		border-radius: var(--m-radius-md);
		padding: 5px 12px;
		cursor: pointer;
	}
	button.primary {
		background: var(--m-accent);
		border: 1px solid var(--m-accent);
		color: var(--m-bg);
	}
	button.primary:disabled {
		opacity: 0.6;
		cursor: default;
	}
	button.secondary {
		flex: none;
		background: var(--m-bg-2);
		border: 1px solid var(--m-border);
		color: var(--m-fg);
	}
	.link-button {
		background: none;
		border: none;
		padding: 0;
		font: inherit;
		font-size: var(--m-text-sm);
		color: var(--m-fg-muted);
		cursor: pointer;
	}
	.link-button.danger:hover {
		color: var(--m-danger);
	}
	.qr {
		align-self: center;
		display: flex;
		background: #fff;
		padding: 10px;
		border-radius: var(--m-radius-md);
	}
	.qr :global(svg) {
		width: 200px;
		height: 200px;
	}
	.link-row {
		display: flex;
		align-items: center;
		gap: 8px;
	}
	.link {
		flex: 1;
		min-width: 0;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
		font-size: var(--m-text-xs);
		color: var(--m-fg-muted);
	}
	.fingerprint {
		word-break: break-all;
		font-size: var(--m-text-xs);
	}
	.devices {
		list-style: none;
		margin: 0;
		padding: 0;
		border: 1px solid var(--m-border);
		border-radius: var(--m-radius-md);
	}
	.devices li {
		display: flex;
		align-items: center;
		gap: 8px;
		padding: 6px 10px;
	}
	.devices li + li {
		border-top: 1px solid var(--m-border);
	}
	.device-label {
		flex: 1;
		min-width: 0;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.device-meta {
		color: var(--m-fg-subtle);
		font-size: var(--m-text-xs);
	}
	.enroll {
		display: flex;
		flex-direction: column;
		gap: 10px;
	}
	.enroll label {
		display: flex;
		flex-direction: column;
		gap: 4px;
	}
	.field-label {
		font-size: var(--m-text-xs);
		color: var(--m-fg-subtle);
	}
	.enroll input {
		font: inherit;
		background: var(--m-bg);
		color: var(--m-fg);
		border: 1px solid var(--m-border);
		border-radius: var(--m-radius-md);
		padding: 6px 8px;
	}
	.enroll input:focus {
		outline: none;
		border-color: var(--m-accent);
	}
	footer {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 12px;
		padding-top: 10px;
		border-top: 1px solid var(--m-border);
	}
</style>
