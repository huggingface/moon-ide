<script lang="ts">
	import { join, pathPicker } from '../pathPicker.svelte';
	import { remote } from '../remote.svelte';

	const listing = $derived(pathPicker.listing);
	const title = $derived(pathPicker.mode === 'folder' ? 'Choose a folder' : 'Choose a file');

	function onKeydown(event: KeyboardEvent) {
		if (event.key === 'Escape') {
			event.preventDefault();
			pathPicker.cancel();
		}
	}

	function onPathKeydown(event: KeyboardEvent) {
		if (event.key !== 'Enter') {
			return;
		}
		event.preventDefault();
		void pathPicker.browse(pathPicker.pathInput.trim() || null);
	}

	function focusOnMount(node: HTMLInputElement) {
		queueMicrotask(() => node.focus());
	}
</script>

{#if pathPicker.mode !== null}
	<div class="backdrop" role="presentation" onclick={() => pathPicker.cancel()} onkeydown={onKeydown} tabindex="-1">
		<div
			class="modal"
			role="dialog"
			aria-modal="true"
			aria-labelledby="path-picker-title"
			onclick={(e) => e.stopPropagation()}
			onkeydown={onKeydown}
			tabindex="-1"
		>
			<h2 id="path-picker-title">{title}</h2>
			<p class="hint">On {remote.target?.host}. Type a path and press Enter to jump to it.</p>
			<input
				type="text"
				bind:value={pathPicker.pathInput}
				onkeydown={onPathKeydown}
				use:focusOnMount
				autocomplete="off"
				spellcheck="false"
			/>
			{#if pathPicker.error}
				<p class="error" role="alert">{pathPicker.error}</p>
			{/if}
			<div class="list">
				{#if listing}
					{#if listing.parent}
						<button type="button" class="entry" onclick={() => void pathPicker.browse(listing.parent)}>../</button>
					{/if}
					{#each listing.dirs as dir (dir)}
						<button type="button" class="entry" onclick={() => pathPicker.enter(dir)}>{dir}/</button>
					{/each}
					{#if pathPicker.mode === 'file'}
						{#each listing.files as file (file)}
							<button type="button" class="entry file" onclick={() => pathPicker.choose(join(listing.path, file))}
								>{file}</button
							>
						{/each}
					{/if}
					{#if listing.dirs.length === 0 && (pathPicker.mode === 'folder' || listing.files.length === 0)}
						<p class="empty">Nothing here.</p>
					{/if}
				{:else if pathPicker.loading}
					<p class="empty">Loading…</p>
				{/if}
			</div>
			<div class="actions">
				<button type="button" class="ghost" onclick={() => pathPicker.cancel()}>Cancel</button>
				{#if pathPicker.mode === 'folder'}
					<button
						type="button"
						class="primary"
						disabled={listing === null}
						onclick={() => listing && pathPicker.choose(listing.path)}>Choose this folder</button
					>
				{/if}
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
		max-height: 80vh;
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
	.list {
		max-height: 40vh;
		overflow-y: auto;
		border: 1px solid var(--m-border);
		border-radius: 6px;
		display: flex;
		flex-direction: column;
	}
	.entry {
		text-align: left;
		background: transparent;
		border-radius: 0;
		color: var(--m-fg);
		padding: 4px 10px;
		font-family: var(--m-font-mono, monospace);
		font-size: 12px;
	}
	.entry:hover:not(:disabled) {
		background: var(--m-bg-2);
	}
	.entry.file {
		color: var(--m-fg-muted);
	}
	.empty {
		padding: 8px 10px;
		font-size: 12px;
		color: var(--m-fg-muted);
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
