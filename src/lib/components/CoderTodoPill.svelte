<script lang="ts">
	// Compact at-a-glance indicator of the agent's session-scoped
	// todo list, sat next to the context ring in the panel header.
	// Reads the list from `coder.todos` (mirrored from every
	// successful `todo_write` tool result). Hidden when the list
	// is empty so the header stays clean.
	//
	// Click expands a popover — same shape as the per-call body in
	// `ToolBodyTodoWrite.svelte`, just rendered inline in a
	// floating panel anchored under the pill. Closes on outside
	// click or Escape.
	import { coder, type TodoItem } from '../coder.svelte';

	let open = $state(false);
	let wrap: HTMLDivElement | undefined = $state(undefined);

	const todos = $derived<TodoItem[]>(coder.todos);
	const hasTodos = $derived(todos.length > 0);

	const counts = $derived.by(() => {
		let inProgress = 0;
		let completed = 0;
		let cancelled = 0;
		let pending = 0;
		for (const t of todos) {
			if (t.status === 'in_progress') {
				inProgress += 1;
			} else if (t.status === 'completed') {
				completed += 1;
			} else if (t.status === 'cancelled') {
				cancelled += 1;
			} else {
				pending += 1;
			}
		}
		return { inProgress, completed, cancelled, pending };
	});

	// Dominant glyph for the pill: in_progress beats fully-done,
	// fully-done beats neutral. Drives both the leading character
	// and the colour class on `.pill`.
	type DominantState = 'in_progress' | 'all_done' | 'pending';
	const dominant = $derived<DominantState>(
		counts.inProgress > 0
			? 'in_progress'
			: counts.completed + counts.cancelled === todos.length && todos.length > 0
				? 'all_done'
				: 'pending',
	);

	// Auto-close when the list goes back to empty (e.g. agent
	// cleared the plan with `merge: false` + `todos: []`). Keeps
	// us from leaving a popover anchored to a button that's about
	// to disappear.
	$effect(() => {
		if (!hasTodos && open) {
			open = false;
		}
	});

	// Outside-click and Escape close the popover. `wrap` includes
	// the trigger button itself so clicking the pill toggles
	// rather than re-opens.
	$effect(() => {
		if (!open) {
			return;
		}
		const onPointerDown = (event: PointerEvent) => {
			if (wrap && wrap.contains(event.target as Node)) {
				return;
			}
			open = false;
		};
		const onKey = (event: KeyboardEvent) => {
			if (event.key === 'Escape') {
				open = false;
			}
		};
		window.addEventListener('pointerdown', onPointerDown);
		window.addEventListener('keydown', onKey);
		return () => {
			window.removeEventListener('pointerdown', onPointerDown);
			window.removeEventListener('keydown', onKey);
		};
	});

	function tooltipFor(): string {
		const total = todos.length;
		const done = counts.completed + counts.cancelled;
		if (counts.inProgress > 0) {
			return `Todo list — ${counts.inProgress} in progress, ${done} / ${total} done`;
		}
		if (dominant === 'all_done') {
			return `Todo list — ${total} item${total === 1 ? '' : 's'}, all done`;
		}
		return `Todo list — ${done} / ${total} done`;
	}
</script>

{#if hasTodos}
	<div class="todo-wrap" bind:this={wrap}>
		<button
			type="button"
			class="pill"
			class:in_progress={dominant === 'in_progress'}
			class:all_done={dominant === 'all_done'}
			aria-label={tooltipFor()}
			aria-expanded={open}
			title={tooltipFor()}
			onclick={() => (open = !open)}
		>
			<span class="glyph" aria-hidden="true">
				{#if dominant === 'in_progress'}
					▶
				{:else if dominant === 'all_done'}
					✓
				{:else}
					○
				{/if}
			</span>
			<span class="count">{counts.completed + counts.cancelled}/{todos.length}</span>
		</button>
		{#if open}
			<div class="popover" role="dialog" aria-label="Todo list">
				<ul class="list">
					{#each todos as todo (todo.id)}
						<li
							class="item"
							class:status-pending={todo.status === 'pending'}
							class:status-in_progress={todo.status === 'in_progress'}
							class:status-completed={todo.status === 'completed'}
							class:status-cancelled={todo.status === 'cancelled'}
						>
							<span class="g" aria-hidden="true">
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
							<span class="c">{todo.content}</span>
						</li>
					{/each}
				</ul>
			</div>
		{/if}
	</div>
{/if}

<style>
	.todo-wrap {
		position: relative;
		display: flex;
		align-items: center;
	}
	/* Compact pill — same vertical rhythm as `ContextRing` so the
	   two indicators sit happily side-by-side. The glyph carries
	   the dominant state at a glance; the `M/N` count gives a
	   precise readout without needing to open the popover. */
	.pill {
		display: flex;
		align-items: center;
		gap: 4px;
		padding: 2px 6px;
		font-size: 11px;
		font-variant-numeric: tabular-nums;
		background: var(--m-bg-overlay);
		color: var(--m-fg-muted);
		border: 1px solid transparent;
		border-radius: 999px;
		cursor: pointer;
		line-height: 1.2;
	}
	.pill:hover,
	.pill:focus-visible {
		color: var(--m-fg);
		border-color: var(--m-border, transparent);
		outline: none;
	}
	.pill.in_progress {
		color: var(--m-accent);
	}
	.pill.in_progress .glyph {
		color: var(--m-accent);
	}
	.pill.all_done {
		color: var(--m-success, var(--m-fg-muted));
	}
	.pill.all_done .glyph {
		color: var(--m-success, var(--m-fg-muted));
	}
	.glyph {
		font-family: var(--m-font-mono, ui-monospace, monospace);
	}
	.count {
		font-family: var(--m-font-mono, ui-monospace, monospace);
	}
	/* Popover anchored under the pill. `right: 0` so a long list
	   grows leftward instead of clipping off the panel on narrow
	   widths. Width caps + max-height keep a chatty agent's plan
	   from eating the whole header. Uses `--m-bg-2` (the same
	   solid surface the status-bar popovers use) — the previous
	   `--m-bg-elevated` fallback to `--m-bg-overlay` rendered
	   ~3 % alpha and let coder-row text bleed through. */
	.popover {
		position: absolute;
		top: calc(100% + 6px);
		right: 0;
		z-index: 20;
		min-width: 240px;
		max-width: min(360px, 90vw);
		max-height: 360px;
		overflow: auto;
		background: var(--m-bg-2);
		border: 1px solid var(--m-border-strong, var(--m-border, transparent));
		border-radius: 6px;
		box-shadow: 0 6px 24px rgba(0, 0, 0, 0.5);
		padding: 6px 0;
	}
	.list {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: 2px;
	}
	.item {
		display: flex;
		align-items: baseline;
		gap: 8px;
		padding: 3px 10px;
		font-size: 12px;
		line-height: 1.4;
	}
	.g {
		flex: 0 0 1.2em;
		text-align: center;
		font-family: var(--m-font-mono, ui-monospace, monospace);
		color: var(--m-fg-subtle);
		user-select: none;
	}
	.c {
		flex: 1 1 auto;
		min-width: 0;
		color: var(--m-fg);
	}
	.item.status-in_progress .g,
	.item.status-in_progress .c {
		color: var(--m-accent);
		font-weight: 500;
	}
	.item.status-completed .c,
	.item.status-cancelled .c {
		color: var(--m-fg-subtle);
		text-decoration: line-through;
		text-decoration-thickness: 1px;
	}
	.item.status-completed .g {
		color: var(--m-success, var(--m-accent));
	}
</style>
