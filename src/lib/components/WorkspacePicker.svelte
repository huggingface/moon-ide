<script lang="ts">
	import { workspacePicker } from '../workspacePicker.svelte';
	import { currentWorkspaceId } from '../workspace-id';

	function onBackdropKeydown(event: KeyboardEvent) {
		if (event.key === 'Escape') {
			event.preventDefault();
			workspacePicker.close();
		}
	}

	function onModalKeydown(event: KeyboardEvent) {
		if (event.key === 'Escape') {
			event.preventDefault();
			workspacePicker.close();
			return;
		}
		if (event.key === 'ArrowDown') {
			event.preventDefault();
			workspacePicker.moveSelection(1);
			return;
		}
		if (event.key === 'ArrowUp') {
			event.preventDefault();
			workspacePicker.moveSelection(-1);
			return;
		}
		if (event.key === 'Enter') {
			event.preventDefault();
			const list = workspacePicker.filtered;
			const meta = list[workspacePicker.selectedIndex];
			if (meta) {
				void workspacePicker.activate(meta);
			}
		}
	}

	function focusOnMount(node: HTMLInputElement) {
		queueMicrotask(() => node.focus());
	}

	function formatRelative(ts: number): string {
		if (ts <= 0) {
			return 'never';
		}
		const now = Math.floor(Date.now() / 1000);
		const delta = Math.max(0, now - ts);
		if (delta < 60) {
			return 'just now';
		}
		if (delta < 3600) {
			return `${Math.floor(delta / 60)}m ago`;
		}
		if (delta < 86400) {
			return `${Math.floor(delta / 3600)}h ago`;
		}
		return `${Math.floor(delta / 86400)}d ago`;
	}
</script>

{#if workspacePicker.visible}
	<div
		class="backdrop"
		role="presentation"
		onclick={() => workspacePicker.close()}
		onkeydown={onBackdropKeydown}
		tabindex="-1"
	>
		<div
			class="modal"
			role="dialog"
			aria-modal="true"
			aria-label="Switch workspace"
			onclick={(e) => e.stopPropagation()}
			onkeydown={onModalKeydown}
			tabindex="-1"
		>
			<input
				type="text"
				placeholder="Switch workspace…"
				bind:value={workspacePicker.query}
				use:focusOnMount
				autocomplete="off"
				spellcheck="false"
			/>
			{#if workspacePicker.error}
				<p class="error" role="alert">{workspacePicker.error}</p>
			{/if}
			<div class="rows" role="listbox" aria-label="Workspaces">
				{#each workspacePicker.filtered as meta, i (meta.id)}
					{@const isCurrent = meta.id === currentWorkspaceId()}
					{@const isSelected = i === workspacePicker.selectedIndex}
					<div class="row" class:selected={isSelected} role="option" aria-selected={isSelected} tabindex="-1">
						<button
							type="button"
							class="row-main"
							onclick={() => void workspacePicker.activate(meta)}
							onmouseenter={() => (workspacePicker.selectedIndex = i)}
						>
							<div class="row-name">
								{meta.name}
								{#if isCurrent}
									<span class="badge">current</span>
								{/if}
							</div>
							<div class="row-meta">
								<span class="slug">{meta.id}</span>
								<span class="dot">·</span>
								<span>{formatRelative(meta.last_active_at)}</span>
							</div>
						</button>
						{#if !isCurrent}
							<button
								type="button"
								class="forget"
								title="Forget this workspace"
								onclick={(e) => {
									e.stopPropagation();
									void workspacePicker.forget(meta);
								}}
							>
								Forget
							</button>
						{/if}
					</div>
				{:else}
					<p class="empty">No workspaces match "{workspacePicker.query}".</p>
				{/each}
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
		padding: 12px;
		width: min(540px, 92vw);
		display: flex;
		flex-direction: column;
		gap: 8px;
		color: var(--m-fg);
		max-height: 70vh;
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
	.rows {
		display: flex;
		flex-direction: column;
		gap: 2px;
		overflow-y: auto;
		min-height: 0;
	}
	.row {
		display: flex;
		align-items: center;
		gap: 4px;
		border-radius: 6px;
		padding: 0;
	}
	.row.selected {
		background: var(--m-bg-2);
	}
	.row-main {
		flex: 1;
		text-align: left;
		background: transparent;
		border: 0;
		padding: 8px 10px;
		font-family: inherit;
		font-size: 13px;
		color: var(--m-fg);
		cursor: pointer;
		display: flex;
		flex-direction: column;
		gap: 2px;
		min-width: 0;
	}
	.row-name {
		display: flex;
		align-items: center;
		gap: 8px;
		font-weight: 500;
	}
	.badge {
		background: var(--m-accent);
		color: #0d1017;
		font-size: 10px;
		font-weight: 600;
		padding: 1px 6px;
		border-radius: 999px;
	}
	.row-meta {
		display: flex;
		align-items: center;
		gap: 6px;
		font-size: 11px;
		color: var(--m-fg-muted);
	}
	.slug {
		font-family: var(--m-font-mono);
	}
	.dot {
		opacity: 0.5;
	}
	.forget {
		background: transparent;
		border: 1px solid var(--m-border);
		color: var(--m-fg-muted);
		font-size: 11px;
		padding: 4px 8px;
		border-radius: 4px;
		font-family: inherit;
		cursor: pointer;
		margin-right: 6px;
	}
	.forget:hover {
		background: var(--m-bg-2);
		color: var(--m-fg-danger, #ff6b6b);
		border-color: var(--m-fg-danger, #ff6b6b);
	}
	.empty {
		margin: 0;
		padding: 16px;
		text-align: center;
		font-size: 12px;
		color: var(--m-fg-muted);
	}
</style>
