<script lang="ts">
	// The agent's plan as a step chain above the composer, after
	// chat-ui's ML assistant strip: one dot per todo (done filled,
	// running pulsing, pending hollow), joined by connectors that fill
	// in behind finished steps, then the running step's text. Click
	// expands the full checklist. Shown while the plan has open steps
	// or the turn is still running.
	import { coder } from '../coder.svelte';
	import type { TodoItem } from '../coder.svelte';
	import TodoList from './TodoList.svelte';

	const todos = $derived<TodoItem[]>(coder.todos);
	const settledCount = $derived(todos.filter(settled).length);
	const complete = $derived(todos.length > 0 && settledCount === todos.length);
	const current = $derived(todos.find((t) => t.status === 'in_progress'));
	const visible = $derived(todos.length > 0 && (!complete || coder.busy));
	const statusText = $derived(complete ? 'Done' : (current?.content ?? `${settledCount}/${todos.length} done`));
	let open = $state(false);

	function settled(todo: TodoItem): boolean {
		return todo.status === 'completed' || todo.status === 'cancelled';
	}
</script>

{#if visible}
	<div class="plan-strip">
		{#if open}
			<div class="plan-strip-list">
				<TodoList {todos} />
			</div>
		{/if}
		<button
			type="button"
			class="plan-strip-bar"
			aria-expanded={open}
			title={open ? 'Hide plan' : 'Show plan'}
			onclick={() => (open = !open)}
		>
			<span class="steps" role="list" aria-label="Plan">
				{#each todos as todo, i (todo.id)}
					{#if i > 0}
						<span class="connector" class:settled={settled(todos[i - 1] ?? todo)} aria-hidden="true"></span>
					{/if}
					<span
						role="listitem"
						class="step"
						data-status={todo.status}
						title={`${i + 1}. ${todo.content}`}
						aria-label={`Step ${i + 1}, ${todo.content}: ${todo.status.replace('_', ' ')}`}
					>
						{#if todo.status === 'completed'}
							<svg viewBox="0 0 12 12" aria-hidden="true">
								<path
									d="M2.5 6.5l2.2 2.2L9.5 3.8"
									fill="none"
									stroke="currentColor"
									stroke-width="2"
									stroke-linecap="round"
									stroke-linejoin="round"
								/>
							</svg>
						{:else if todo.status === 'cancelled'}
							<span class="dash"></span>
						{/if}
					</span>
				{/each}
			</span>
			<span class="status" class:complete title={statusText}>{statusText}</span>
		</button>
	</div>
{/if}

<style>
	.plan-strip {
		display: flex;
		flex-direction: column;
		gap: 6px;
		min-width: 0;
	}
	.plan-strip-list {
		max-height: 240px;
		overflow: auto;
		padding: 2px 2px 4px;
	}
	.plan-strip-bar {
		display: flex;
		align-items: center;
		gap: 12px;
		min-width: 0;
		padding: 0;
		border: none;
		background: none;
		font: inherit;
		text-align: left;
		cursor: pointer;
	}
	.steps {
		--step: 12px;
		display: flex;
		flex: 0 1 auto;
		align-items: center;
		min-width: 0;
		overflow: hidden;
	}
	.connector {
		flex: 0 1 10px;
		min-width: 2px;
		height: 1.5px;
		background: var(--m-border);
	}
	.connector.settled {
		background: var(--m-accent);
	}
	.step {
		box-sizing: border-box;
		flex: none;
		display: grid;
		place-items: center;
		width: var(--step);
		height: var(--step);
		border-radius: 50%;
		border: 1.5px solid var(--m-border-strong, var(--m-fg-subtle));
		color: var(--m-bg);
	}
	.step svg {
		width: 9px;
		height: 9px;
	}
	.step[data-status='completed'] {
		border-color: var(--m-accent);
		background: var(--m-accent);
	}
	.step[data-status='cancelled'] {
		border-color: var(--m-accent);
	}
	.dash {
		width: 6px;
		height: 1.5px;
		border-radius: 1px;
		background: var(--m-accent);
	}
	/* Running: a solid core with a ring that breathes past the slot. */
	.step[data-status='in_progress'] {
		position: relative;
		border: none;
	}
	.step[data-status='in_progress']::before {
		content: '';
		position: absolute;
		inset: 0;
		border-radius: 50%;
		background: var(--m-accent);
		animation: plan-step-pulse 1.4s ease-out infinite;
	}
	.step[data-status='in_progress']::after {
		content: '';
		width: calc(var(--step) / 2);
		height: calc(var(--step) / 2);
		border-radius: 50%;
		background: var(--m-accent);
	}
	@keyframes plan-step-pulse {
		0% {
			transform: scale(0.55);
			opacity: 0.55;
		}
		100% {
			transform: scale(1.5);
			opacity: 0;
		}
	}
	@media (prefers-reduced-motion: reduce) {
		.step[data-status='in_progress']::before {
			animation: none;
			opacity: 0.25;
		}
	}
	.status {
		flex: 1 1 auto;
		min-width: 0;
		overflow: hidden;
		white-space: nowrap;
		text-overflow: ellipsis;
		font-size: 12px;
		color: var(--m-fg-muted);
	}
	.status.complete {
		color: var(--m-accent);
		font-weight: 600;
	}
	.plan-strip-bar:hover .status {
		color: var(--m-fg);
	}
</style>
