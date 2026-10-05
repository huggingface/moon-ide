<script lang="ts">
	// Tool body for `open_terminal` (ADR 0090): the command, which
	// side it runs on, a button to bring its tab forward while it's
	// still open, and the output the agent waited for, if any.
	import { bottomPanel } from '../bottomPanel.svelte';
	import { fmtJson, parseToolError } from './toolBodyHelpers';

	interface Props {
		args: unknown;
		result: unknown;
		hasResult: boolean;
	}

	let { args, result, hasResult }: Props = $props();

	const resultErr = $derived(hasResult ? parseToolError(result) : null);
	/** Match the success shape of `tools.rs::open_terminal`:
	 *  `{ id, target, command, reused, output?, matched? }`. */
	function parseResult(r: unknown): {
		id: string;
		target: string;
		command: string;
		reused: boolean;
		output: string | null;
		matched: boolean | null;
	} | null {
		if (typeof r !== 'object' || r === null) {
			return null;
		}
		const { id, target, command, reused, output, matched } = r as Record<string, unknown>;
		if (typeof id !== 'string' || typeof command !== 'string') {
			return null;
		}
		return {
			id,
			target: typeof target === 'string' ? target : '',
			command,
			reused: reused === true,
			output: typeof output === 'string' ? output : null,
			matched: typeof matched === 'boolean' ? matched : null,
		};
	}

	const opened = $derived(hasResult && resultErr === null ? parseResult(result) : null);
	const stillOpen = $derived(opened !== null && bottomPanel.tabs.some((t) => t.id === opened.id));

	function reveal(id: string): void {
		bottomPanel.show();
		bottomPanel.setActive(id);
	}
</script>

{#if resultErr !== null}
	<div class="ot-error">{resultErr}</div>
{:else if opened !== null}
	<div class="ot-row">
		<button
			type="button"
			class="ot-cmd tool-link"
			disabled={!stillOpen}
			title={stillOpen ? 'Show terminal' : 'Terminal was closed'}
			onclick={() => reveal(opened.id)}
		>
			{opened.command}
		</button>
		<span class="ot-meta">
			{opened.target}{opened.reused ? ' · reused' : ''}{opened.matched === false ? ' · wait timed out' : ''}{stillOpen
				? ''
				: ' · closed'}
		</span>
	</div>
	{#if opened.output !== null && opened.output.length > 0}
		<pre class="ot-output">{opened.output}</pre>
	{/if}
{:else}
	<div class="block-label">args</div>
	<pre class="block">{fmtJson(args)}</pre>
{/if}

<style>
	.ot-row {
		display: flex;
		gap: 8px;
		align-items: baseline;
		margin-top: 4px;
		font-size: 11px;
	}
	.ot-cmd {
		flex: 1 1 auto;
		min-width: 0;
		background: transparent;
		border: 0;
		padding: 0;
		text-align: left;
		font-family: var(--m-font-mono, ui-monospace, monospace);
		color: var(--m-accent, var(--m-fg));
		cursor: pointer;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.ot-cmd:hover:not(:disabled) {
		text-decoration: underline;
	}
	.ot-cmd:disabled {
		color: var(--m-fg-muted);
		cursor: default;
	}
	.ot-meta {
		color: var(--m-fg-subtle);
	}
	.ot-output {
		background: var(--m-bg);
		color: var(--m-fg);
		border-radius: 4px;
		padding: 6px 8px;
		max-height: 240px;
		overflow: auto;
		font-family: var(--m-font-mono, ui-monospace, monospace);
		font-size: 11px;
		line-height: 1.4;
		margin: 4px 0 0;
		white-space: pre-wrap;
		word-break: break-word;
	}
	.ot-error {
		margin-top: 4px;
		font-size: 11px;
		color: var(--m-danger);
	}
</style>
