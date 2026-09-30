<script lang="ts">
	// Tool body for `open_browser` (ADR 0088): the URL, where it
	// resolves, and a button to bring the tab back — focused if still
	// open, reopened (same URL + side) if it was closed.
	import { ipc } from '../ipc';
	import { fmtJson, parseToolError } from './toolBodyHelpers';

	interface Props {
		args: unknown;
		result: unknown;
		hasResult: boolean;
	}

	let { args, result, hasResult }: Props = $props();

	const resultErr = $derived(hasResult ? parseToolError(result) : null);
	/** Match the success shape of `tools.rs::open_browser`:
	 *  `{ tab_id, url, target, reused }`. */
	function parseResult(r: unknown): { url: string; inContainer: boolean } | null {
		if (typeof r !== 'object' || r === null) {
			return null;
		}
		const { url, target } = r as { url?: unknown; target?: unknown };
		if (typeof url !== 'string') {
			return null;
		}
		return { url, inContainer: target === 'container' };
	}

	const opened = $derived(hasResult && resultErr === null ? parseResult(result) : null);
</script>

{#if resultErr !== null}
	<div class="ob-error">{resultErr}</div>
{:else if opened !== null}
	<div class="ob-row">
		<button
			type="button"
			class="ob-url tool-link"
			title="Open in IDE browser tab"
			onclick={() => void ipc.browser.open(opened.url, opened.inContainer, true)}
		>
			{opened.url}
		</button>
		<span class="ob-meta">{opened.inContainer ? 'container' : 'host'}</span>
	</div>
{:else}
	<div class="block-label">args</div>
	<pre class="block">{fmtJson(args)}</pre>
{/if}

<style>
	.ob-row {
		display: flex;
		gap: 8px;
		align-items: baseline;
		margin-top: 4px;
		font-size: 11px;
	}
	.ob-url {
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
	.ob-url:hover {
		text-decoration: underline;
	}
	.ob-meta {
		color: var(--m-fg-subtle);
	}
	.ob-error {
		margin-top: 4px;
		font-size: 11px;
		color: var(--m-danger);
	}
</style>
