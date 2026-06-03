<script lang="ts">
	import { app } from './app.svelte';

	// The phone gets here either by scanning the desktop's QR (which
	// fills these fields via the URL hash) or by typing the payload
	// in. We parse a pasted JSON payload too, so a user can paste the
	// whole QR string the desktop prints.
	let url = $state('');
	let code = $state('');
	let pasted = $state('');
	let busy = $state(false);

	const label = `${navigator.platform || 'phone'} companion`;

	function applyPasted(): void {
		const text = pasted.trim();
		if (!text) {
			return;
		}
		try {
			const payload = JSON.parse(text) as { url?: string; code?: string };
			if (payload.url) {
				url = payload.url;
			}
			if (payload.code) {
				code = payload.code;
			}
		} catch {
			// Not JSON — leave the typed fields as-is.
		}
	}

	async function submit(): Promise<void> {
		busy = true;
		await app.pair(url.trim(), code.trim(), label);
		busy = false;
	}

	const canSubmit = $derived(url.trim().length > 0 && code.trim().length > 0 && !busy);
</script>

<div class="screen">
	<h1>Pair with moon-ide</h1>
	<p class="muted">
		On your computer, open the Companion panel in moon-ide and run the bridge. Paste the pairing payload it shows, or
		type the URL and code.
	</p>

	<div class="card list">
		<label for="paste">Paste pairing payload</label>
		<input id="paste" bind:value={pasted} oninput={applyPasted} placeholder={'{"url":"wss://…","code":"…"}'} />
	</div>

	<div class="card list">
		<label for="url">Bridge URL</label>
		<input id="url" bind:value={url} placeholder="wss://192.168.1.20:53180" autocomplete="off" />
		<label for="code">Pairing code</label>
		<input id="code" bind:value={code} placeholder="A1B2-C3D4" autocomplete="off" />
	</div>

	{#if app.error}
		<p class="error">{app.error}</p>
	{/if}

	<button class="primary" disabled={!canSubmit} onclick={submit}>
		{busy ? 'Pairing…' : 'Pair'}
	</button>

	<p class="muted">
		Your phone must trust the bridge's certificate first (the desktop shows a fingerprint and a one-time trust profile).
	</p>
</div>
