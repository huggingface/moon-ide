<script lang="ts">
	// Tool body for the `web_fetch` tool. The result is a chunk of
	// markdown extracted from the target page by Jina Reader — we
	// render it through the same `CoderMarkdown` pipeline an
	// assistant message would, so headings / links / code blocks
	// pick up the IDE's styling and any URLs the page exposes
	// remain externally clickable.
	import { openUrl } from '@tauri-apps/plugin-opener';

	import CoderMarkdown from './CoderMarkdown.svelte';
	import { fmtJson, parseToolError } from './toolBodyHelpers';

	interface Props {
		args: unknown;
		result: unknown;
		hasResult: boolean;
	}

	let { args, result, hasResult }: Props = $props();

	/** Match `crates/moon-coder/src/tools.rs`'s `WebFetchArgs`. */
	function parseArgs(a: unknown): { url: string } | null {
		if (typeof a !== 'object' || a === null) {
			return null;
		}
		const o = a as { url?: unknown };
		if (typeof o.url !== 'string') {
			return null;
		}
		return { url: o.url };
	}

	/** Match the success-shape `json!` block in `tools.rs::web_fetch`:
	 *  `{ url, markdown, truncated, bytes }`. */
	function parseResult(r: unknown): { url: string | null; markdown: string; truncated: boolean; bytes: number } | null {
		if (typeof r !== 'object' || r === null) {
			return null;
		}
		const o = r as Record<string, unknown>;
		if (typeof o.markdown !== 'string') {
			return null;
		}
		return {
			url: typeof o.url === 'string' ? o.url : null,
			markdown: o.markdown,
			truncated: o.truncated === true,
			bytes: typeof o.bytes === 'number' ? o.bytes : o.markdown.length,
		};
	}

	const argsP = $derived(parseArgs(args));
	const resultErr = $derived(hasResult ? parseToolError(result) : null);
	const resultP = $derived(hasResult && resultErr === null ? parseResult(result) : null);
	const parseable = $derived(argsP !== null || resultP !== null || resultErr !== null);
	const url = $derived(resultP?.url ?? argsP?.url ?? null);

	/** Display the byte count using the MB / kB / 1000-multiple
	 *  convention from `AGENTS.md`. Smaller payloads stay raw bytes
	 *  so a 280-byte snippet doesn't read as "0 kB". */
	function fmtBytes(n: number): string {
		if (n < 1_000) {
			return `${n} B`;
		}
		if (n < 1_000_000) {
			return `${Math.round(n / 1_000)} kB`;
		}
		return `${(n / 1_000_000).toFixed(1)} MB`;
	}
</script>

{#if !parseable}
	<div class="block-label">args</div>
	<pre class="block">{fmtJson(args)}</pre>
	{#if hasResult}
		<div class="block-label">result</div>
		<pre class="block">{fmtJson(result)}</pre>
	{/if}
{:else}
	<div class="wf-block">
		<header class="wf-header">
			{#if url !== null}
				<button
					type="button"
					class="wf-url tool-link"
					title={`Open ${url} in browser`}
					onclick={() => void openUrl(url)}
				>
					{url}
				</button>
			{/if}
			{#if resultP !== null}
				<span class="wf-meta">
					{fmtBytes(resultP.bytes)}
					{#if resultP.truncated}· truncated{/if}
				</span>
			{/if}
		</header>
		{#if resultErr !== null}
			<div class="wf-error">{resultErr}</div>
		{:else if resultP !== null}
			<!-- The fetched markdown rendered through the same
				 pipeline an assistant message uses. Capped height
				 so a 200 kB doc page doesn't push the entire
				 transcript page-tall; the body scrolls inside. -->
			<div class="wf-body">
				<CoderMarkdown text={resultP.markdown} />
			</div>
		{/if}
	</div>
{/if}

<style>
	.wf-block {
		display: flex;
		flex-direction: column;
		gap: 4px;
		margin-top: 4px;
	}
	.wf-header {
		display: flex;
		gap: 8px;
		align-items: baseline;
		font-size: 11px;
		flex-wrap: wrap;
	}
	.wf-url {
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
	.wf-url:hover {
		text-decoration: underline;
	}
	.wf-meta {
		flex: 0 0 auto;
		color: var(--m-fg-subtle);
		font-variant-numeric: tabular-nums;
	}
	.wf-error {
		font-size: 11px;
		color: var(--m-error, #d34c4c);
		background: var(--m-bg);
		border-radius: 4px;
		padding: 6px 8px;
	}
	.wf-body {
		background: var(--m-bg);
		border-radius: 4px;
		padding: 8px 12px;
		max-height: 480px;
		overflow: auto;
		font-size: 12px;
	}
</style>
