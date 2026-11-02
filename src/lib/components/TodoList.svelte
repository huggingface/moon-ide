<script lang="ts">
	// The agent's todo list as a checklist: status icon + text, styled
	// after chat-ui's plan card (done items fade instead of striking
	// through; only cancelled ones are struck). The plan strip's
	// expanded view.
	import type { TodoItem } from '../coder.svelte';

	type Props = { todos: TodoItem[] };
	let { todos }: Props = $props();
</script>

<ol class="todo-list">
	{#each todos as todo (todo.id)}
		<li class="todo-item" data-status={todo.status}>
			<span class="todo-icon" aria-hidden="true">
				{#if todo.status === 'completed'}
					<svg
						viewBox="0 0 16 16"
						fill="none"
						stroke="currentColor"
						stroke-width="1.8"
						stroke-linecap="round"
						stroke-linejoin="round"
					>
						<path d="M3.5 8.5l3 3 6-7" />
					</svg>
				{:else if todo.status === 'in_progress'}
					<svg viewBox="0 0 16 16">
						<circle cx="8" cy="8" r="6" fill="none" stroke="currentColor" stroke-width="1.5" />
						<path d="M8 2a6 6 0 0 1 0 12z" fill="currentColor" />
					</svg>
				{:else if todo.status === 'cancelled'}
					<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round">
						<path d="M4 8h8" />
					</svg>
				{:else}
					<svg viewBox="0 0 16 16">
						<circle cx="8" cy="8" r="6" fill="none" stroke="currentColor" stroke-width="1.5" />
					</svg>
				{/if}
			</span>
			<span class="todo-text">{todo.content}</span>
		</li>
	{/each}
</ol>

<style>
	.todo-list {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: 4px;
	}
	.todo-item {
		display: flex;
		align-items: flex-start;
		gap: 6px;
		min-width: 0;
		font-size: 12px;
		line-height: 1.45;
		color: var(--m-fg-muted);
	}
	.todo-icon {
		flex: 0 0 auto;
		display: flex;
		width: 13px;
		height: 13px;
		margin-top: 2px;
		color: var(--m-fg-subtle);
	}
	.todo-icon svg {
		width: 100%;
		height: 100%;
	}
	.todo-text {
		min-width: 0;
		overflow-wrap: anywhere;
	}
	.todo-item[data-status='in_progress'] {
		color: var(--m-fg);
		font-weight: 500;
	}
	.todo-item[data-status='in_progress'] .todo-icon {
		color: var(--m-accent);
	}
	.todo-item[data-status='completed'] {
		color: var(--m-fg-subtle);
	}
	.todo-item[data-status='completed'] .todo-icon {
		color: var(--m-success);
	}
	.todo-item[data-status='cancelled'] {
		color: var(--m-fg-subtle);
		text-decoration: line-through;
	}
</style>
