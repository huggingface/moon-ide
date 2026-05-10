<script lang="ts">
	// Empty-catalog landing page. The only thing this process is
	// allowed to do is collect a workspace name from the user;
	// submitting spawns a real `--workspace <slug>` child and
	// exits this preboot. The screen intentionally has no other
	// chrome — no sidebar, no menu, no shortcuts beyond
	// Enter on the form — so the user can't get into a state
	// where preboot is doing useful work it doesn't have the
	// backend wiring for.
	//
	// Submit / spawn / exit-after-spawn logic lives in the
	// shared `workspaceCreate` store; this component is just
	// the full-page presentation. Escape is intentionally not
	// wired (there's nothing to escape to).

	import { workspaceCreate } from '../workspaceCreate.svelte';

	function focusOnMount(node: HTMLInputElement) {
		queueMicrotask(() => node.focus());
	}

	function onKeydown(event: KeyboardEvent) {
		if (event.key === 'Enter' && !event.shiftKey && !event.ctrlKey) {
			event.preventDefault();
			void workspaceCreate.submit();
		}
	}
</script>

<div class="landing">
	<div class="card">
		<h1>Welcome to moon-ide</h1>
		<p class="hint">Name your first workspace. Each workspace opens in its own window with its own folders.</p>
		<input
			type="text"
			placeholder="e.g. Hugging Face"
			bind:value={workspaceCreate.name}
			disabled={workspaceCreate.busy}
			use:focusOnMount
			onkeydown={onKeydown}
			autocomplete="off"
			spellcheck="false"
		/>
		{#if workspaceCreate.error}
			<p class="error" role="alert">{workspaceCreate.error}</p>
		{/if}
		<div class="actions">
			<button
				type="button"
				class="primary"
				onclick={() => void workspaceCreate.submit()}
				disabled={workspaceCreate.busy}
			>
				{workspaceCreate.busy ? 'Creating…' : 'Create workspace'}
			</button>
		</div>
	</div>
</div>

<style>
	.landing {
		display: flex;
		align-items: center;
		justify-content: center;
		height: 100vh;
		background: var(--m-bg);
		color: var(--m-fg);
	}
	.card {
		background: var(--m-bg-1);
		border: 1px solid var(--m-border-strong);
		border-radius: 8px;
		box-shadow: 0 12px 40px rgba(0, 0, 0, 0.5);
		padding: 32px 36px;
		width: min(440px, 90vw);
		display: flex;
		flex-direction: column;
		gap: 12px;
	}
	h1 {
		margin: 0;
		font-size: 20px;
		font-weight: 600;
	}
	.hint {
		margin: 0;
		font-size: 13px;
		color: var(--m-fg-muted);
	}
	input {
		background: var(--m-bg-2);
		border: 1px solid var(--m-border);
		border-radius: 6px;
		padding: 10px 12px;
		font-size: 14px;
		color: var(--m-fg);
		font-family: inherit;
		margin-top: 6px;
	}
	input:focus {
		outline: 2px solid var(--m-accent);
		outline-offset: -1px;
	}
	.error {
		margin: 0;
		font-size: 12px;
		color: var(--m-fg-danger, #ff6b6b);
	}
	.actions {
		display: flex;
		justify-content: flex-end;
		margin-top: 4px;
	}
	button {
		font-size: 13px;
		padding: 8px 16px;
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
