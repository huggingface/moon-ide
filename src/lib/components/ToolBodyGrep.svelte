<script lang="ts">
	import { fmtJson, openToolPath } from './toolBodyHelpers';

	interface Props {
		args: unknown;
		result: unknown;
		hasResult: boolean;
	}

	let { args, result, hasResult }: Props = $props();

	/** Match `crates/moon-coder/src/tools.rs`'s `GrepArgs`. */
	function parseArgs(a: unknown): { pattern: string; caseSensitive: boolean; maxMatches: number | null } | null {
		if (typeof a !== 'object' || a === null) {
			return null;
		}
		const o = a as { pattern?: unknown; case_sensitive?: unknown; max_matches?: unknown };
		if (typeof o.pattern !== 'string') {
			return null;
		}
		return {
			pattern: o.pattern,
			caseSensitive: o.case_sensitive === true,
			maxMatches: typeof o.max_matches === 'number' ? o.max_matches : null,
		};
	}

	/** Match the `json!` block in `tools.rs::grep`. The shape is a
	 *  flat string (`matches`) of `path:line: text\n` rows plus a
	 *  count and truncation flag. We split per-row in `parseHits`
	 *  for the structured render; the JSON-fallback path uses the
	 *  raw object. */
	function parseResult(
		r: unknown,
	): { pattern: string | null; matches: string; count: number | null; truncated: boolean } | null {
		if (typeof r !== 'object' || r === null) {
			return null;
		}
		const o = r as Record<string, unknown>;
		if (typeof o.matches !== 'string') {
			return null;
		}
		return {
			pattern: typeof o.pattern === 'string' ? o.pattern : null,
			matches: o.matches,
			count: typeof o.count === 'number' ? o.count : null,
			truncated: o.truncated === true,
		};
	}

	/** Split the formatted `path:line: text` body into per-row
	 *  records. The `path` may itself contain `:` on Windows-style
	 *  drive letters (`C:\\…`) — we walk to the *second* `:` to
	 *  split off the line number, then a `: ` to split off the
	 *  text. Lines that don't match the expected shape get rendered
	 *  as raw text rows so the user still sees something useful
	 *  for legacy / error formats. */
	function parseHits(matches: string): Array<{ path: string; line: number; text: string } | { raw: string }> {
		if (matches.length === 0) {
			return [];
		}
		const trimmed = matches.endsWith('\n') ? matches.slice(0, -1) : matches;
		const out: Array<{ path: string; line: number; text: string } | { raw: string }> = [];
		for (const row of trimmed.split('\n')) {
			// Path can contain `:` (drive letter, sub-resources, …).
			// Walk forward to find the *first* `:` followed by an
			// all-digits run terminated by `: ` — that's the split
			// point between path and line number.
			const m = /^(.+?):(\d+):\s?(.*)$/.exec(row);
			if (m === null) {
				out.push({ raw: row });
				continue;
			}
			const [, path, lineStr, text] = m;
			const line = Number.parseInt(lineStr ?? '', 10);
			if (!path || Number.isNaN(line)) {
				out.push({ raw: row });
				continue;
			}
			out.push({ path, line, text: text ?? '' });
		}
		return out;
	}

	const argsP = $derived(parseArgs(args));
	const resultP = $derived(hasResult ? parseResult(result) : null);
	const parseable = $derived(argsP !== null || resultP !== null);
	const pattern = $derived(resultP?.pattern ?? argsP?.pattern ?? null);
	const hits = $derived(resultP !== null ? parseHits(resultP.matches) : []);
</script>

{#if !parseable}
	<div class="block-label">args</div>
	<pre class="block">{fmtJson(args)}</pre>
	{#if hasResult}
		<div class="block-label">result</div>
		<pre class="block">{fmtJson(result)}</pre>
	{/if}
{:else}
	<div class="grep-block">
		<header class="grep-header">
			{#if pattern !== null}
				<span class="grep-pattern" title={pattern}>{pattern}</span>
			{/if}
			{#if argsP?.caseSensitive}
				<span class="grep-flag" title="case-sensitive search">case</span>
			{/if}
			{#if resultP !== null}
				<span class="grep-meta">
					{resultP.count ?? hits.length} match{(resultP.count ?? hits.length) === 1 ? '' : 'es'}{#if resultP.truncated}
						· truncated
					{/if}
				</span>
			{/if}
		</header>
		{#if resultP !== null}
			{#if hits.length === 0}
				<div class="grep-empty">no matches</div>
			{:else}
				<!-- Per-hit list. Each row reads `path:line  text` —
					 same shape as `grep -n`, which is what the model
					 itself sees. The path / line gets a subtle accent
					 colour so the eye can scan the column at a glance,
					 the matched text stays plain monospace so a code
					 snippet looks like code rather than chrome. -->
				<div class="grep-hits">
					{#each hits as hit, idx (idx)}
						{#if 'path' in hit}
							<div class="grep-hit">
								<!-- Each `path:line` location is a button that
									 jumps to the file at the matched line.
									 Native `<button>` rather than `<a href>`
									 because we navigate via the workspace
									 store, not URL routing — keeps tab /
									 split-pane / pending-jump semantics in
									 sync with the rest of the IDE. -->
								<button
									type="button"
									class="grep-hit-loc tool-link"
									title={`Open ${hit.path}:${hit.line}`}
									onclick={() => void openToolPath(hit.path, hit.line)}
								>
									<span class="grep-hit-path">{hit.path}</span><span class="grep-hit-sep">:</span><span
										class="grep-hit-line">{hit.line}</span
									>
								</button>
								<span class="grep-hit-text">{hit.text}</span>
							</div>
						{:else}
							<div class="grep-hit grep-hit-raw">{hit.raw}</div>
						{/if}
					{/each}
				</div>
			{/if}
		{/if}
	</div>
{/if}

<style>
	.grep-block {
		display: flex;
		flex-direction: column;
		gap: 4px;
		margin-top: 4px;
	}
	.grep-header {
		display: flex;
		flex-wrap: wrap;
		gap: 8px;
		align-items: baseline;
		font-size: 11px;
	}
	.grep-pattern {
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
	.grep-flag {
		flex: 0 0 auto;
		font-size: 10px;
		text-transform: uppercase;
		letter-spacing: 0.06em;
		color: var(--m-fg-subtle);
		border: 1px solid var(--m-border);
		border-radius: 3px;
		padding: 0 4px;
	}
	.grep-meta {
		flex: 1 1 auto;
		color: var(--m-fg-subtle);
	}
	.grep-empty {
		font-size: 11px;
		color: var(--m-fg-subtle);
		font-style: italic;
		padding: 6px 8px;
		background: var(--m-bg);
		border-radius: 4px;
	}
	/* Hit list. We deliberately render each row as plain DOM (not a
	   single `<pre>`) so the path/line/text columns can carry their
	   own colour without an extra HTML escape pass. The whole block
	   is scrollable so a 200-hit ripgrep result doesn't push the
	   transcript page-tall — capping at 360px matches the read_file
	   body for visual consistency. */
	.grep-hits {
		display: flex;
		flex-direction: column;
		background: var(--m-bg);
		border-radius: 4px;
		max-height: 360px;
		overflow: auto;
		font-family: var(--m-font-mono, ui-monospace, monospace);
		font-size: 11px;
		line-height: 1.4;
		padding: 4px 0;
	}
	.grep-hit {
		display: flex;
		flex-wrap: nowrap;
		gap: 8px;
		padding: 1px 8px;
		white-space: pre;
	}
	/* The button itself inherits the global `.tool-link` chrome
	   (link-like accent + underline on hover). We override the
	   default colour to `--m-fg-subtle` so only the path span
	   inside picks up the saturated accent — the `:` separator
	   and line number stay muted, which keeps the path scannable
	   in a long list of hits. */
	.grep-hit-loc {
		flex: 0 0 auto;
		color: var(--m-fg-subtle);
	}
	.grep-hit-path {
		color: var(--m-accent, var(--m-fg));
	}
	.grep-hit-sep {
		color: var(--m-fg-subtle);
	}
	.grep-hit-line {
		color: var(--m-fg-subtle);
		font-variant-numeric: tabular-nums;
	}
	.grep-hit-text {
		flex: 1 1 auto;
		min-width: 0;
		color: var(--m-fg);
		overflow: hidden;
		text-overflow: ellipsis;
	}
	.grep-hit-raw {
		color: var(--m-fg-subtle);
		font-style: italic;
	}
</style>
