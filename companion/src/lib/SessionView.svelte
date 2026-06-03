<script lang="ts">
	import { app } from './app.svelte';

	let draft = $state('');

	async function send(): Promise<void> {
		const text = draft.trim();
		if (!text) {
			return;
		}
		draft = '';
		await app.sendPrompt(text);
	}

	function onKeydown(e: KeyboardEvent): void {
		// Enter sends; Shift+Enter newline (matches the desktop composer).
		if (e.key === 'Enter' && !e.shiftKey) {
			e.preventDefault();
			void send();
		}
	}
</script>

<div class="session">
	<div class="row session-head">
		<button class="ghost" onclick={() => app.closeSession()}>← Sessions</button>
		{#if app.busy}
			<span class="muted">running…</span>
		{/if}
	</div>

	<div class="transcript">
		{#each app.rows as row (row.kind + row.id)}
			{#if row.kind === 'user'}
				<div class="bubble user">{row.text}</div>
			{:else if row.kind === 'assistant'}
				<div class="bubble assistant">{row.text}</div>
			{:else}
				<div class="tool" class:error={row.status === 'error'}>
					<span class="pip" class:live={row.status === 'running'}></span>
					{row.name}
					{#if row.status === 'done'}✓{:else if row.status === 'error'}✗{/if}
				</div>
			{/if}
		{/each}
		{#if app.rows.length === 0}
			<p class="muted">No messages yet. Send one below.</p>
		{/if}
	</div>

	{#if app.error}
		<p class="error">{app.error}</p>
	{/if}

	<div class="composer">
		<textarea bind:value={draft} onkeydown={onKeydown} placeholder="Message the coder — Enter to send" rows="2"
		></textarea>
		{#if app.busy}
			<button class="ghost" onclick={() => app.abort()}>Stop</button>
		{:else}
			<button class="primary" onclick={send} disabled={!draft.trim()}>Send</button>
		{/if}
	</div>
</div>

<style>
	.session {
		display: flex;
		flex-direction: column;
		height: 100vh;
		padding: 0.75rem;
		gap: 0.5rem;
	}
	.session-head {
		justify-content: space-between;
	}
	.transcript {
		flex: 1;
		overflow-y: auto;
		display: flex;
		flex-direction: column;
		gap: 0.5rem;
		padding: 0.25rem;
	}
	.bubble {
		padding: 0.5rem 0.7rem;
		border-radius: var(--radius);
		white-space: pre-wrap;
		word-break: break-word;
	}
	.bubble.user {
		background: var(--bg-elev-2);
		align-self: flex-end;
		max-width: 85%;
	}
	.bubble.assistant {
		background: var(--bg-elev);
		border: 1px solid var(--border);
	}
	.tool {
		font-size: 0.85rem;
		color: var(--fg-muted);
		display: flex;
		align-items: center;
		gap: 0.4rem;
	}
	.tool.error {
		color: var(--danger);
	}
	.composer {
		display: flex;
		gap: 0.5rem;
		align-items: flex-end;
	}
	.composer textarea {
		flex: 1;
		resize: none;
		font: inherit;
		background: var(--bg-elev);
		color: var(--fg);
		border: 1px solid var(--border);
		border-radius: var(--radius);
		padding: 0.5rem;
	}
</style>
