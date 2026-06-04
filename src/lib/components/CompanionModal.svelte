<script lang="ts">
	import { onMount, onDestroy } from 'svelte';
	import { renderSVG } from 'uqr';
	import { ipc } from '../ipc';
	import { companion } from '../companion.svelte';

	// The store owns the polled status (shared with the status-bar
	// pip); the modal just renders it and keeps a poll ref alive while
	// open so it refreshes a touch faster after a pair/revoke.
	const status = $derived(companion.status);

	onMount(() => {
		companion.startPolling();
		void companion.refresh();
	});

	onDestroy(() => {
		companion.stopPolling();
	});

	// The QR encodes the full pairing payload (url + fingerprint + code)
	// that the PWA's pair screen parses from a paste/scan.
	const qrSvg = $derived(status?.pairing_payload ? renderSVG(status.pairing_payload, { border: 2 }) : null);

	async function revoke(id: string): Promise<void> {
		await ipc.companion.revokeDevice(id);
		// The bridge picks up the request within ~1s; nudge a refresh.
		setTimeout(() => void companion.refresh(), 1200);
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

<div class="overlay" role="dialog" aria-modal="true" aria-label="Pair a phone">
	<div class="card">
		<header>
			<h2>Companion</h2>
			<button type="button" class="close" aria-label="Close" onclick={() => companion.close()}>×</button>
		</header>

		{#if !status || !status.running}
			<p class="lede">The companion bridge isn't running yet.</p>
			<p class="hint">
				It starts automatically with a release build of moon-ide. In a dev session, run
				<code>moon-bridge serve --web-root companion/dist</code> in a terminal, then reopen this dialog.
			</p>
		{:else}
			<p class="lede">
				On your phone (same network/VPN), open the companion and scan this, or type the address + code.
			</p>

			{#if qrSvg}
				<!-- eslint-disable-next-line svelte/no-at-html-tags -->
				<div class="qr">{@html qrSvg}</div>
			{/if}

			<div class="details">
				{#if status.mdns_url}
					<div class="row"><span class="k">Address</span><code>{status.mdns_url}</code></div>
					<div class="row"><span class="k">or</span><code>{status.pairing_url}</code></div>
				{:else}
					<div class="row"><span class="k">Address</span><code>{status.pairing_url}</code></div>
				{/if}
				{#if status.pairing_code}
					<div class="row"><span class="k">Code</span><code class="code">{status.pairing_code}</code></div>
				{:else}
					<p class="hint">Pairing window closed — restart the bridge to pair another device.</p>
				{/if}
			</div>

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
</style>
