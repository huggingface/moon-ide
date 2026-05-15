<script lang="ts">
	// Tool body for the `todo_write` tool. Renders the canonical
	// post-merge list (lifted from `result.todos`, falling back to
	// `args.todos` while the call is in flight) with status glyphs
	// and strikethrough on completed / cancelled items. The header
	// pill in `CoderPanel.svelte` reads the same payload from the
	// per-folder `coder.todos` bucket; both surfaces stay in
	// lock-step because the runner re-emits the canonical list on
	// every successful call.
	import type { TodoItem } from '../coder.svelte';
	import { fmtJson } from './toolBodyHelpers';

	interface Props {
		args: unknown;
		result: unknown;
		hasResult: boolean;
	}

	let { args, result, hasResult }: Props = $props();

	const TODO_STATUSES: ReadonlySet<string> = new Set(['pending', 'in_progress', 'completed', 'cancelled']);

	function isTodoStatus(value: unknown): value is TodoItem['status'] {
		return typeof value === 'string' && TODO_STATUSES.has(value);
	}

	/** Pull a `TodoItem[]` out of either an args payload (which
	 *  the model sends) or a result payload (which the runner
	 *  echoes back as the canonical post-merge list). Returns
	 *  `null` when the shape doesn't match so the caller falls
	 *  back to the JSON view. Tolerates the same field-presence
	 *  variations the runner accepts. */
	function parseTodos(value: unknown): TodoItem[] | null {
		if (typeof value !== 'object' || value === null) {
			return null;
		}
		const raw = (value as { todos?: unknown }).todos;
		if (!Array.isArray(raw)) {
			return null;
		}
		// `Array.isArray` widens to `any[]`; re-assert to
		// `unknown[]` so the per-item narrowing below starts from
		// `unknown` and oxlint doesn't flag the cast as unsafe.
		const items: unknown[] = raw;
		const out: TodoItem[] = [];
		for (const item of items) {
			if (typeof item !== 'object' || item === null) {
				return null;
			}
			const o = item as { id?: unknown; content?: unknown; status?: unknown };
			if (typeof o.id !== 'string' || typeof o.content !== 'string') {
				return null;
			}
			if (!isTodoStatus(o.status)) {
				return null;
			}
			out.push({ id: o.id, content: o.content, status: o.status });
		}
		return out;
	}

	const resultTodos = $derived(hasResult ? parseTodos(result) : null);
	const argsTodos = $derived(parseTodos(args));
	// Prefer the result (canonical post-merge list) once it's in;
	// fall back to args while the call is mid-flight so the user
	// sees the proposal before the runner echoes it back. If
	// neither parses, we punt to the JSON fallback below.
	const todos = $derived<TodoItem[] | null>(resultTodos ?? argsTodos);
	const argsParsed = $derived<{ merge: boolean } | null>(
		typeof args === 'object' && args !== null && 'merge' in args
			? { merge: (args as { merge: unknown }).merge === true }
			: null,
	);
</script>

{#if todos === null}
	<div class="block-label">args</div>
	<pre class="block">{fmtJson(args)}</pre>
	{#if hasResult}
		<div class="block-label">result</div>
		<pre class="block">{fmtJson(result)}</pre>
	{/if}
{:else}
	<div class="todo-block">
		{#if todos.length === 0}
			<!-- Empty post-merge list — typically the agent calling
				 `merge: false` with `todos: []` to clear the plan.
				 Worth showing rather than collapsing entirely so the
				 user sees the deliberate clear in the transcript. -->
			<div class="todo-empty">{argsParsed?.merge ? 'no changes' : 'list cleared'}</div>
		{:else}
			<ul class="todo-list">
				{#each todos as todo (todo.id)}
					<li
						class="todo-item"
						class:status-pending={todo.status === 'pending'}
						class:status-in_progress={todo.status === 'in_progress'}
						class:status-completed={todo.status === 'completed'}
						class:status-cancelled={todo.status === 'cancelled'}
					>
						<span class="todo-glyph" aria-hidden="true">
							{#if todo.status === 'pending'}
								○
							{:else if todo.status === 'in_progress'}
								▶
							{:else if todo.status === 'completed'}
								✓
							{:else}
								−
							{/if}
						</span>
						<span class="todo-content">{todo.content}</span>
					</li>
				{/each}
			</ul>
		{/if}
	</div>
{/if}

<style>
	.todo-block {
		display: flex;
		flex-direction: column;
		gap: 4px;
		margin-top: 4px;
	}
	.todo-empty {
		font-size: 11px;
		color: var(--m-fg-subtle);
		font-style: italic;
		padding: 6px 8px;
		background: var(--m-bg);
		border-radius: 4px;
	}
	.todo-list {
		list-style: none;
		margin: 0;
		padding: 4px 0;
		background: var(--m-bg);
		border-radius: 4px;
		display: flex;
		flex-direction: column;
		gap: 2px;
	}
	.todo-item {
		display: flex;
		align-items: baseline;
		gap: 8px;
		padding: 2px 8px;
		font-size: 12px;
		line-height: 1.4;
	}
	.todo-glyph {
		flex: 0 0 1.2em;
		text-align: center;
		font-family: var(--m-font-mono, ui-monospace, monospace);
		color: var(--m-fg-subtle);
		user-select: none;
	}
	.todo-content {
		flex: 1 1 auto;
		min-width: 0;
		color: var(--m-fg);
	}
	/* In-progress accent matches the running-tool dot's accent so
	   the agent's current focus reads as one visual identity
	   regardless of which surface (todo body, header pill, tool
	   row dot) you're looking at. */
	.todo-item.status-in_progress .todo-glyph,
	.todo-item.status-in_progress .todo-content {
		color: var(--m-accent);
		font-weight: 500;
	}
	.todo-item.status-completed .todo-content,
	.todo-item.status-cancelled .todo-content {
		color: var(--m-fg-subtle);
		text-decoration: line-through;
		text-decoration-thickness: 1px;
	}
	.todo-item.status-completed .todo-glyph {
		color: var(--m-success, var(--m-accent));
	}
</style>
