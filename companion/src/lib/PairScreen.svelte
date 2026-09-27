<script lang="ts">
	import { onMount } from 'svelte';
	import { app } from './app.svelte';
	import { parsePairLink } from './e2e';

	// The phone gets here by scanning a pairing QR — a link to this very
	// page with everything in the fragment (relay routing code, the IDE's
	// public key, a one-time secret; ADR 0087) — or by pasting that same
	// link, e.g. copied out of `moon-remote pair` over ssh. There is no
	// short typed code: both paths carry the full secret.
	let pasted = $state('');
	let busy = $state(false);

	const label = `${navigator.platform || 'phone'} companion`;
	const link = $derived(parsePairLink(pasted, window.location.origin));

	onMount(() => {
		const href = window.location.href;
		if (!parsePairLink(href)) {
			return;
		}
		// Drop the single-use secret from the address bar / history
		// before anything else.
		history.replaceState(null, '', window.location.pathname);
		void run(href);
	});

	async function run(text: string): Promise<void> {
		const parsed = parsePairLink(text, window.location.origin);
		if (!parsed) {
			return;
		}
		busy = true;
		await app.pair(parsed, label);
		busy = false;
	}
</script>

<div class="screen">
	<h1>Pair with moon-ide</h1>
	<p class="muted">
		Scan the pairing QR from moon-ide's Companion panel (or from <code>moon-remote pair</code> on a server), or paste its
		link below.
	</p>

	<div class="card list">
		<label for="paste">Pairing link</label>
		<input id="paste" bind:value={pasted} placeholder={'https://…/#pair=…&k=…&s=…'} autocomplete="off" />
		{#if pasted.trim() && !link}
			<p class="error">That doesn't look like a pairing link — copy the whole link, including everything after #.</p>
		{/if}
	</div>

	{#if app.error}
		<p class="error">{app.error}</p>
	{/if}

	<button class="primary" disabled={!link || busy} onclick={() => run(pasted)}>
		{busy ? 'Pairing…' : 'Pair'}
	</button>

	<p class="muted">
		The link carries the IDE's key and a one-time secret: the relay in between can route traffic but can't read it or
		pair itself in your place.
	</p>
</div>
