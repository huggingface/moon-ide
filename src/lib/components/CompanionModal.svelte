<script lang="ts">
	import { onMount, onDestroy } from 'svelte';
	import { renderSVG } from 'uqr';
	import { ipc, type PairingQr } from '../ipc';
	import { formatError } from '../protocol';
	import { companion } from '../companion.svelte';

	// The store owns the polled status (shared with the status-bar
	// pip); the modal just renders it and keeps a poll ref alive while
	// open so it refreshes a touch faster after a pair/revoke.
	const status = $derived(companion.status);

	// Tab: "local" = this machine runs the bridge (Phase 13 QR flow);
	// "remote" = the IDE dials out to a relay bridge elsewhere (14.3).
	// Defaults to local; auto-switches to remote if already connected.
	let tab = $state<'local' | 'remote'>('local');

	onMount(() => {
		companion.startPolling();
		void companion.refresh();
		void companion.refreshRemote().then(() => {
			if (companion.remoteStatus?.connected) {
				tab = 'remote';
			}
		});
	});

	onDestroy(() => {
		companion.stopPolling();
	});

	// Local-bridge pairing is on-demand (Phase 14.5): the button mints
	// a fresh single-use code over the control socket. The QR encodes
	// the full payload (url + fingerprint + code) that the PWA's pair
	// screen parses from a paste/scan.
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
	let enrollUrl = $state('');
	let enrollCode = $state('');
	let enrollError = $state<string | null>(null);
	let enrolling = $state(false);

	async function enrollRemote(event: Event): Promise<void> {
		event.preventDefault();
		enrollError = null;
		enrolling = true;
		try {
			await ipc.companion.enroll(enrollUrl, enrollCode, 'moon-ide');
			await companion.refreshRemote();
			enrollUrl = '';
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

<div class="overlay" role="dialog" aria-modal="true" aria-label="Companion">
	<div class="card">
		<header>
			<h2>Companion</h2>
			<button type="button" class="close" aria-label="Close" onclick={() => companion.close()}>×</button>
		</header>

		<div class="tabs" role="tablist">
			<button
				type="button"
				role="tab"
				aria-selected={tab === 'local'}
				class="tab"
				class:active={tab === 'local'}
				onclick={() => (tab = 'local')}
			>
				Local bridge
			</button>
			<button
				type="button"
				role="tab"
				aria-selected={tab === 'remote'}
				class="tab"
				class:active={tab === 'remote'}
				onclick={() => {
					tab = 'remote';
					void companion.refreshRemote();
				}}
			>
				Remote relay
			</button>
		</div>

		{#if tab === 'local'}
			<!-- Local bridge: this machine runs moon-bridge, phones pair to it. -->
			{#if !status || !status.running}
				<p class="lede">The companion bridge isn't running yet.</p>
				<p class="hint">
					It starts automatically with a release build of moon-ide. In a dev session, run
					<code>moon-bridge serve --web-root companion/dist</code> in a terminal, then reopen this dialog.
				</p>
			{:else}
				<p class="lede">
					On your phone (same network/VPN), open the companion and scan a pairing QR, or type the address + code.
				</p>

				{#if localPair}
					{#if qrSvg}
						<!-- eslint-disable-next-line svelte/no-at-html-tags -->
						<div class="qr">{@html qrSvg}</div>
					{/if}
					<div class="details">
						{#if status.mdns_url}
							<div class="row"><span class="k">Address</span><code>{status.mdns_url}</code></div>
							<div class="row"><span class="k">or</span><code>{localPair.url}</code></div>
						{:else}
							<div class="row"><span class="k">Address</span><code>{localPair.url}</code></div>
						{/if}
						<div class="row"><span class="k">Code</span><code class="code">{localPair.code}</code></div>
					</div>
					<p class="hint">Single-use, valid ~2 minutes. Generate a new one per phone.</p>
				{:else}
					<div class="details">
						<div class="row"><span class="k">Address</span><code>{status.mdns_url ?? status.url}</code></div>
					</div>
				{/if}
				<button type="button" onclick={() => requestLocalPairCode()}>
					{localPair ? 'New pairing code' : 'Show pairing QR'}
				</button>
				{#if localPairError}
					<p class="hint" style="color: var(--danger, #f85149)">{localPairError}</p>
				{/if}

				<p class="hint fp">
					First time on a device, accept the self-signed certificate. Fingerprint:<br />
					<code class="fingerprint">{status.fingerprint}</code>
				</p>

				<h3>Paired devices</h3>
				{#if status.devices.length === 0}
					<p class="hint">None yet.</p>
				{:else}
					<ul class="devices">
						{#each status.devices as d (d.id)}
							<li>
								<span class="label">{d.label}</span>
								<span class="meta">{relativeTime(d.paired_at_ms)}</span>
								<button type="button" class="revoke" onclick={() => revoke(d.id)}>Revoke</button>
							</li>
						{/each}
					</ul>
				{/if}

				{#if status.ides && status.ides.length > 0}
					<h3>Enrolled IDEs</h3>
					<ul class="devices">
						{#each status.ides as ide (ide.id)}
							<li>
								<span class="label">{ide.label}</span>
								<span class="meta">{relativeTime(ide.enrolled_at_ms)}</span>
								<button type="button" class="revoke" onclick={() => revokeIde(ide.id)}>Revoke</button>
							</li>
						{/each}
					</ul>
				{/if}
			{/if}
		{:else}
			<!-- Remote relay: the IDE dials out to a bridge elsewhere. -->
			{#if companion.remoteStatus?.connected}
				<p class="lede">
					Connected to <code>{companion.remoteStatus.bridge_url}</code>
				</p>
				{#if companion.remoteStatus.error}
					<p class="hint" style="color: var(--danger, #f85149)">{companion.remoteStatus.error}</p>
				{/if}
				<p class="hint">
					This IDE is enrolled as <code>{companion.remoteStatus.ide_id}</code>. Phones paired to the remote bridge can
					see this IDE's workspaces.
				</p>

				<h3>Pair a phone</h3>
				{#if remotePair}
					{#if remotePairQrSvg}
						<!-- eslint-disable-next-line svelte/no-at-html-tags -->
						<div class="qr">{@html remotePairQrSvg}</div>
					{/if}
					<div class="details">
						<div class="row"><span class="k">Address</span><code>{remotePair.url}</code></div>
						<div class="row"><span class="k">Code</span><code class="code">{remotePair.code}</code></div>
					</div>
					<p class="hint">Single-use, valid ~2 minutes. Generate a new one per phone.</p>
				{:else}
					<p class="hint">Mint a fresh single-use pairing code on the relay and show it as a QR.</p>
				{/if}
				<button type="button" onclick={() => requestRemotePairCode()}>
					{remotePair ? 'New pairing code' : 'Show pairing QR'}
				</button>
				{#if remotePairError}
					<p class="hint" style="color: var(--danger, #f85149)">{remotePairError}</p>
				{/if}

				<button type="button" class="revoke" onclick={() => disconnectRemote()}>Disconnect</button>
			{:else}
				<p class="lede">Connect this IDE to a remote relay bridge.</p>
				<p class="hint">
					Run <code>moon-bridge serve</code> on the relay box (a machine on the same VPN), note the enrollment code it prints,
					then enter it here.
				</p>
				<form onsubmit={enrollRemote} class="remote-form">
					<input type="text" placeholder="wss://relay-box:53180" bind:value={enrollUrl} required disabled={enrolling} />
					<input type="text" placeholder="enrollment code" bind:value={enrollCode} required disabled={enrolling} />
					<button type="submit" disabled={enrolling}>{enrolling ? 'Enrolling…' : 'Enroll'}</button>
				</form>
				{#if enrollError}
					<p class="hint" style="color: var(--danger, #f85149)">{enrollError}</p>
				{/if}
			{/if}
		{/if}
	</div>
</div>

<style>
	.overlay {
		position: fixed;
		inset: 0;
		background: rgba(0, 0, 0, 0.5);
		display: flex;
		align-items: center;
		justify-content: center;
		z-index: 1000;
	}
	.card {
		background: var(--surface, #161b22);
		color: var(--fg, #e6edf3);
		border: 1px solid var(--border, #30363d);
		border-radius: 10px;
		padding: 1.25rem;
		width: min(420px, 92vw);
		max-height: 90vh;
		overflow-y: auto;
	}
	header {
		display: flex;
		justify-content: space-between;
		align-items: center;
	}
	h2 {
		margin: 0;
		font-size: 1.1rem;
	}
	h3 {
		font-size: 0.95rem;
		margin: 1rem 0 0.4rem;
	}
	.close {
		background: none;
		border: none;
		color: var(--fg-muted, #8b949e);
		font-size: 1.4rem;
		cursor: pointer;
		line-height: 1;
	}
	.lede {
		color: var(--fg, #e6edf3);
		margin: 0.5rem 0;
	}
	.hint {
		color: var(--fg-muted, #8b949e);
		font-size: 0.85rem;
	}
	.qr {
		display: flex;
		justify-content: center;
		background: #fff;
		padding: 0.75rem;
		border-radius: 8px;
		margin: 0.75rem 0;
	}
	.qr :global(svg) {
		width: 200px;
		height: 200px;
	}
	.details {
		display: flex;
		flex-direction: column;
		gap: 0.3rem;
	}
	.row {
		display: flex;
		gap: 0.6rem;
		align-items: baseline;
	}
	.k {
		color: var(--fg-muted, #8b949e);
		min-width: 4rem;
		font-size: 0.85rem;
	}
	code {
		font-family: var(--mono, ui-monospace, monospace);
		word-break: break-all;
	}
	.code {
		font-size: 1.1rem;
		letter-spacing: 0.05em;
	}
	.fingerprint {
		font-size: 0.7rem;
		color: var(--fg-muted, #8b949e);
	}
	.fp {
		margin-top: 0.75rem;
	}
	.devices {
		list-style: none;
		padding: 0;
		margin: 0;
		display: flex;
		flex-direction: column;
		gap: 0.4rem;
	}
	.devices li {
		display: flex;
		align-items: center;
		gap: 0.6rem;
	}
	.label {
		flex: 1;
	}
	.meta {
		color: var(--fg-muted, #8b949e);
		font-size: 0.8rem;
	}
	.revoke {
		background: none;
		border: 1px solid var(--border, #30363d);
		color: var(--danger, #f85149);
		border-radius: 6px;
		padding: 0.2rem 0.5rem;
		cursor: pointer;
		font-size: 0.8rem;
	}
	.tabs {
		display: flex;
		gap: 0.25rem;
		margin: 0.75rem 0;
	}
	.tab {
		flex: 1;
		background: none;
		border: 1px solid var(--border, #30363d);
		border-radius: 6px;
		color: var(--fg-muted, #8b949e);
		cursor: pointer;
		padding: 0.4rem 0.5rem;
		font-size: 0.85rem;
	}
	.tab.active {
		background: var(--bg-input, #161b22);
		color: var(--fg, #e6edf3);
		border-color: var(--accent, #388bfd);
	}
	.remote-form {
		display: flex;
		gap: 0.4rem;
		margin-top: 0.5rem;
	}
	.remote-form input {
		flex: 1;
		background: var(--bg-input, #161b22);
		border: 1px solid var(--border, #30363d);
		border-radius: 6px;
		color: var(--fg, #e6edf3);
		padding: 0.3rem 0.5rem;
		font-size: 0.85rem;
	}
	.remote-form button {
		background: var(--accent, #388bfd);
		border: none;
		border-radius: 6px;
		color: white;
		cursor: pointer;
		padding: 0.3rem 0.8rem;
		font-size: 0.85rem;
	}
</style>
