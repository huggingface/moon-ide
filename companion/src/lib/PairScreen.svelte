<script lang="ts">
	import { onMount } from 'svelte';
	import { app } from './app.svelte';

	// The phone gets here by scanning the desktop's QR — a link to
	// this very page with the code in the fragment
	// (`https://<bridge>/#pair=<code>`), so a camera scan lands here
	// and pairing starts by itself — or by typing the URL + code in.
	let url = $state('');
	let code = $state('');
	let pasted = $state('');
	let busy = $state(false);

	const label = `${navigator.platform || 'phone'} companion`;

	// The PWA is served by the bridge itself (directly or behind the
	// relay's TLS front), so the page origin *is* the WS endpoint.
	const originWsUrl = `wss://${window.location.host}`;

	onMount(() => {
		const scanned = /^#pair=([A-Za-z0-9-]+)$/.exec(window.location.hash)?.[1];
		if (!scanned) {
			return;
		}
		// Drop the single-use code from the address bar / history
		// before anything else.
		history.replaceState(null, '', window.location.pathname);
		url = originWsUrl;
		code = scanned;
		void submit();
	});

	function applyPasted(): void {
		const text = pasted.trim();
		if (!text) {
			return;
		}
		// A pasted QR link (`https://…#pair=CODE`) fills both fields.
		const link = /^https:\/\/([^/#?]+)[^#]*#pair=([A-Za-z0-9-]+)$/.exec(text);
		const host = link?.[1];
		const linkCode = link?.[2];
		if (host && linkCode) {
			url = `wss://${host}`;
			code = linkCode;
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
		<label for="paste">Paste pairing link</label>
		<input id="paste" bind:value={pasted} oninput={applyPasted} placeholder={'https://…#pair=A1B2-C3D4'} />
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
