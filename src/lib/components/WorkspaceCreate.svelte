<script lang="ts">
	import { workspaceCreate } from '../workspaceCreate.svelte';

	function onKeydown(event: KeyboardEvent) {
		if (event.key === 'Escape') {
			event.preventDefault();
			workspaceCreate.close();
			return;
		}
		if (event.key === 'Enter' && !event.shiftKey && !event.ctrlKey) {
			event.preventDefault();
			void workspaceCreate.submit();
		}
	}

	function focusOnMount(node: HTMLInputElement) {
		queueMicrotask(() => node.focus());
	}
</script>

{#if workspaceCreate.visible}
	<div
		class="backdrop"
		role="presentation"
		onclick={() => workspaceCreate.close()}
		onkeydown={(e) => {
			if (e.key === 'Escape') {
				workspaceCreate.close();
			}
		}}
		tabindex="-1"
	>
		<div
			class="modal"
			role="dialog"
			aria-modal="true"
			aria-labelledby="workspace-create-title"
			onclick={(e) => e.stopPropagation()}
			onkeydown={onKeydown}
			tabindex="-1"
		>
			<h2 id="workspace-create-title">New workspace</h2>
			<p class="hint">Each workspace opens in its own window with its own folders.</p>
			<input
				type="text"
				placeholder="e.g. Hugging Face"
				bind:value={workspaceCreate.name}
				disabled={workspaceCreate.busy}
				use:focusOnMount
				autocomplete="off"
				spellcheck="false"
			/>
			{#if workspaceCreate.error}
				<p class="error" role="alert">{workspaceCreate.error}</p>
			{/if}
			<div class="actions">
				<button type="button" class="ghost" onclick={() => workspaceCreate.close()} disabled={workspaceCreate.busy}
					>Cancel</button
				>
				<button
					type="button"
					class="primary"
					onclick={() => void workspaceCreate.submit()}
					disabled={workspaceCreate.busy}
				>
					{workspaceCreate.busy ? 'Creating…' : 'Create'}
				</button>
			</div>
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
		padding-top: 12vh;
		z-index: 50;
	}
	.modal {
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
	.hint {
		margin: 0;
		font-size: 12px;
		color: var(--m-fg-muted);
	}
	input {
		background: var(--m-bg-2);
		border: 1px solid var(--m-border);
		border-radius: 6px;
		padding: 8px 10px;
		font-size: 13px;
		color: var(--m-fg);
		font-family: inherit;
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
		gap: 8px;
		justify-content: flex-end;
		margin-top: 4px;
	}
	button {
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
	.ghost {
		background: transparent;
		border-color: var(--m-border);
		color: var(--m-fg-muted);
	}
	.ghost:hover:not(:disabled) {
		background: var(--m-bg-2);
		color: var(--m-fg);
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
