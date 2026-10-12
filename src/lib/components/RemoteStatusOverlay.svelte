<script lang="ts">
	import { remote } from '../remote.svelte';

	const host = $derived(remote.target?.host ?? '');
	const status = $derived(remote.status);
</script>

{#if remote.target !== null && status !== null && status.state !== 'connected'}
	<div class="backdrop" role="presentation">
		<div class="card" role="status" aria-live="polite">
			{#if status.state === 'detached'}
				<h2>Open on another desktop</h2>
				<p>This workspace was attached from another desktop. Its sessions, terminals and containers kept running.</p>
				<button type="button" class="primary" onclick={() => void remote.reattach()} disabled={remote.reattaching}>
					{remote.reattaching ? 'Attaching…' : 'Use it here'}
				</button>
			{:else if status.state === 'disconnected'}
				<h2>Reconnecting to {host}…</h2>
				<p class="error">{status.error}</p>
			{:else}
				<h2>Connecting to {host}…</h2>
				<p>Starting the workspace on the server if it isn't running yet.</p>
			{/if}
		</div>
	</div>
{/if}

<style>
	.backdrop {
		position: fixed;
		inset: 0;
		background: rgba(0, 0, 0, 0.45);
		display: flex;
		align-items: flex-start;
		justify-content: center;
		padding-top: 18vh;
		z-index: 100;
	}
	.card {
		background: var(--m-bg-1);
		border: 1px solid var(--m-border-strong);
		border-radius: 8px;
		box-shadow: 0 12px 40px rgba(0, 0, 0, 0.5);
		padding: 20px 22px;
		width: min(440px, 92vw);
		display: flex;
		flex-direction: column;
		gap: 12px;
		color: var(--m-fg);
	}
	h2 {
		margin: 0;
		font-size: 16px;
		font-weight: 600;
	}
	p {
		margin: 0;
		font-size: 12px;
		color: var(--m-fg-muted);
	}
	.error {
		color: var(--m-fg-danger, #ff6b6b);
		font-family: var(--m-font-mono, monospace);
		word-break: break-word;
	}
	button {
		align-self: flex-end;
		font-size: 13px;
		padding: 6px 12px;
		border-radius: 6px;
		border: 1px solid transparent;
		font-family: inherit;
		cursor: pointer;
	}
	button:disabled {
		opacity: 0.55;
		cursor: progress;
	}
	.primary {
		background: var(--m-accent);
		color: #0d1017;
		font-weight: 600;
	}
	.primary:hover:not(:disabled) {
		background: var(--m-accent-strong);
	}
</style>
