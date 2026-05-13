<script lang="ts">
	// Tool body for the `web_search` tool. Renders the Tavily SERP
	// the agent saw: a header with the query + result count, then
	// one card per result with title, URL, and snippet. External URL
	// click goes through Tauri's `plugin-opener` so it lands in the
	// system browser rather than a no-op `window.open` inside the
	// WebView.
	import { openUrl } from '@tauri-apps/plugin-opener';

	import { fmtJson, parseToolError } from './toolBodyHelpers';

	interface Props {
		args: unknown;
		result: unknown;
		hasResult: boolean;
	}

	let { args, result, hasResult }: Props = $props();

	/** Match `crates/moon-coder/src/tools.rs`'s `WebSearchArgs`. */
	function parseArgs(a: unknown): { query: string; maxResults: number | null } | null {
		if (typeof a !== 'object' || a === null) {
			return null;
		}
		const o = a as { query?: unknown; max_results?: unknown };
		if (typeof o.query !== 'string') {
			return null;
		}
		return {
			query: o.query,
			maxResults: typeof o.max_results === 'number' ? o.max_results : null,
		};
	}

	type Hit = { title: string; url: string; snippet: string; publishedDate: string | null };

	/** Match the success-shape `json!` block in `tools.rs::web_search`:
	 *  `{ query, results: WebSearchResult[], count }`. */
	function parseResult(r: unknown): { query: string | null; hits: Hit[]; count: number } | null {
		if (typeof r !== 'object' || r === null) {
			return null;
		}
		const o = r as Record<string, unknown>;
		const raw = o.results;
		if (!Array.isArray(raw)) {
			return null;
		}
		const hits: Hit[] = [];
		for (const entry of raw) {
			if (typeof entry !== 'object' || entry === null) {
				continue;
			}
			const e = entry as Record<string, unknown>;
			if (typeof e.url !== 'string' || typeof e.title !== 'string' || typeof e.snippet !== 'string') {
				continue;
			}
			hits.push({
				title: e.title,
				url: e.url,
				snippet: e.snippet,
				publishedDate: typeof e.published_date === 'string' ? e.published_date : null,
			});
		}
		return {
			query: typeof o.query === 'string' ? o.query : null,
			hits,
			count: typeof o.count === 'number' ? o.count : hits.length,
		};
	}

	const argsP = $derived(parseArgs(args));
	const resultErr = $derived(hasResult ? parseToolError(result) : null);
	const resultP = $derived(hasResult && resultErr === null ? parseResult(result) : null);
	const parseable = $derived(argsP !== null || resultP !== null || resultErr !== null);
	const query = $derived(resultP?.query ?? argsP?.query ?? null);
</script>

{#if !parseable}
	<div class="block-label">args</div>
	<pre class="block">{fmtJson(args)}</pre>
	{#if hasResult}
		<div class="block-label">result</div>
		<pre class="block">{fmtJson(result)}</pre>
	{/if}
{:else}
	<div class="ws-block">
		<header class="ws-header">
			{#if query !== null}
				<span class="ws-query" title={query}>{query}</span>
			{/if}
			{#if resultP !== null}
				<span class="ws-meta">
					{resultP.count} result{resultP.count === 1 ? '' : 's'}
				</span>
			{/if}
		</header>
		{#if resultErr !== null}
			<!-- Tavily returned an error (bad key, quota, rate limit,
				 etc.). Show it verbatim so the user can tell whether
				 to paste a new key or wait. -->
			<div class="ws-error">{resultErr}</div>
		{:else if resultP !== null}
			{#if resultP.hits.length === 0}
				<div class="ws-empty">no results</div>
			{:else}
				<ul class="ws-hits">
					{#each resultP.hits as hit, idx (idx)}
						<li class="ws-hit">
							<button
								type="button"
								class="ws-title tool-link"
								title={`Open ${hit.url} in browser`}
								onclick={() => void openUrl(hit.url)}
							>
								{hit.title.length > 0 ? hit.title : hit.url}
							</button>
							<div class="ws-url-row">
								<span class="ws-url">{hit.url}</span>
								{#if hit.publishedDate !== null}
									<span class="ws-date">{hit.publishedDate}</span>
								{/if}
							</div>
							{#if hit.snippet.length > 0}
								<p class="ws-snippet">{hit.snippet}</p>
							{/if}
						</li>
					{/each}
				</ul>
			{/if}
		{/if}
	</div>
{/if}

<style>
	.ws-block {
		display: flex;
		flex-direction: column;
		gap: 4px;
		margin-top: 4px;
	}
	.ws-header {
		display: flex;
		flex-wrap: wrap;
		gap: 8px;
		align-items: baseline;
		font-size: 11px;
	}
	.ws-query {
		flex: 0 1 auto;
		min-width: 0;
		font-family: var(--m-font-mono, ui-monospace, monospace);
		color: var(--m-fg);
		background: var(--m-bg);
		padding: 1px 6px;
		border-radius: 3px;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
		max-width: 60ch;
	}
	.ws-meta {
		flex: 1 1 auto;
		color: var(--m-fg-subtle);
	}
	.ws-empty {
		font-size: 11px;
		color: var(--m-fg-subtle);
		font-style: italic;
		padding: 6px 8px;
		background: var(--m-bg);
		border-radius: 4px;
	}
	.ws-error {
		font-size: 11px;
		color: var(--m-error, #d34c4c);
		background: var(--m-bg);
		border-radius: 4px;
		padding: 6px 8px;
	}
	.ws-hits {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: 8px;
		background: var(--m-bg);
		border-radius: 4px;
		padding: 8px;
		max-height: 420px;
		overflow: auto;
	}
	.ws-hit {
		display: flex;
		flex-direction: column;
		gap: 2px;
	}
	.ws-title {
		display: inline-block;
		background: transparent;
		border: 0;
		padding: 0;
		text-align: left;
		font-size: 12px;
		font-weight: 600;
		color: var(--m-accent, var(--m-fg));
		cursor: pointer;
	}
	.ws-title:hover {
		text-decoration: underline;
	}
	.ws-url-row {
		display: flex;
		gap: 8px;
		align-items: baseline;
		font-size: 10.5px;
		color: var(--m-fg-subtle);
		font-family: var(--m-font-mono, ui-monospace, monospace);
	}
	.ws-url {
		flex: 1 1 auto;
		min-width: 0;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.ws-date {
		flex: 0 0 auto;
		font-variant-numeric: tabular-nums;
	}
	.ws-snippet {
		margin: 2px 0 0;
		font-size: 11.5px;
		line-height: 1.4;
		color: var(--m-fg);
	}
</style>
