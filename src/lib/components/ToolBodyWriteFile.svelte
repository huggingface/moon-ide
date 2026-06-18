<script lang="ts">
	import {
		buildLineNumberColumn,
		escapeHtml,
		extractInnerHtml,
		fenceLangFromPath,
		fmtJson,
		highlightCode,
		loadHighlighters,
		openToolPath,
	} from './toolBodyHelpers';
	import ToolReapplyMenu from './ToolReapplyMenu.svelte';

	interface Props {
		args: unknown;
		result: unknown;
		hasResult: boolean;
		/** Tool-call id, for the "re-apply to disk" recovery menu. */
		callId: string;
	}

	let { args, result, hasResult, callId }: Props = $props();

	/** Match `crates/moon-coder/src/tools.rs`'s `WriteFileArgs`. */
	function parseArgs(a: unknown): { path: string; content: string } | null {
		if (typeof a !== 'object' || a === null) {
			return null;
		}
		const o = a as { path?: unknown; content?: unknown };
		if (typeof o.path !== 'string' || typeof o.content !== 'string') {
			return null;
		}
		return { path: o.path, content: o.content };
	}

	function parseResult(
		r: unknown,
	): { path: string | null; bytesWritten: number | null; mtimeMs: number | null } | null {
		if (typeof r !== 'object' || r === null) {
			return null;
		}
		const o = r as Record<string, unknown>;
		// Heuristic: a real write_file success carries `bytes_written`.
		if (typeof o.bytes_written !== 'number') {
			return null;
		}
		return {
			path: typeof o.path === 'string' ? o.path : null,
			bytesWritten: o.bytes_written,
			mtimeMs: typeof o.mtime_ms === 'number' ? o.mtime_ms : null,
		};
	}

	/** kB / MB / GB summary for the bytes-written counter. Matches
	 *  the rest of the codebase's 1000-multiple convention; rounds
	 *  to one decimal so 12_345 reads `12.3 kB` rather than `12kB`. */
	function fmtBytes(n: number): string {
		if (n < 1000) {
			return `${n} B`;
		}
		if (n < 1000_000) {
			return `${(n / 1000).toFixed(1)} kB`;
		}
		if (n < 1000_000_000) {
			return `${(n / 1000_000).toFixed(1)} MB`;
		}
		return `${(n / 1000_000_000).toFixed(2)} GB`;
	}

	const argsP = $derived(parseArgs(args));
	const resultP = $derived(hasResult ? parseResult(result) : null);
	const parseable = $derived(argsP !== null || resultP !== null);
	const path = $derived(resultP?.path ?? argsP?.path ?? null);
	const lang = $derived(fenceLangFromPath(path));
	const content = $derived(argsP?.content ?? '');
	const lineCount = $derived.by(() => {
		if (content.length === 0) {
			return 0;
		}
		// `text.split('\n')` gives `lines + 1` for a trailing
		// newline, which is the same convention as the editor's
		// gutter (one row per `\n`-separated chunk, no phantom
		// trailing row). Strip a single trailing `\n` to match.
		const trimmed = content.endsWith('\n') ? content.slice(0, -1) : content;
		return trimmed.split('\n').length;
	});
	const lineNumbers = $derived(buildLineNumberColumn(1, lineCount));

	let parserReady = $state(false);
	$effect(() => {
		const l = lang;
		if (l === null) {
			parserReady = true;
			return;
		}
		parserReady = false;
		let cancelled = false;
		void loadHighlighters([l]).finally(() => {
			if (!cancelled) {
				parserReady = true;
			}
		});
		return () => {
			cancelled = true;
		};
	});

	const codeForRender = $derived(content.endsWith('\n') ? content.slice(0, -1) : content);
	const codeHtml = $derived.by(() => {
		if (codeForRender.length === 0) {
			return '';
		}
		if (lang !== null && parserReady) {
			const wrapped = highlightCode(codeForRender, lang);
			if (wrapped.length > 0) {
				return extractInnerHtml(wrapped);
			}
		}
		return escapeHtml(codeForRender);
	});
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
			<span class="wf-verb">{resultP !== null ? 'wrote' : 'writing'}</span>
			{#if path !== null}
				<button type="button" class="wf-path tool-link" title={`Open ${path}`} onclick={() => void openToolPath(path)}>
					{path}
				</button>
			{/if}
			{#if resultP !== null && resultP.bytesWritten !== null}
				<span class="wf-meta">{fmtBytes(resultP.bytesWritten)}</span>
			{/if}
			{#if path !== null}
				<ToolReapplyMenu {callId} />
			{/if}
		</header>
		{#if argsP !== null && argsP.content.length > 0}
			<div class="wf-body">
				<pre class="wf-line-numbers" aria-hidden="true">{lineNumbers}</pre>
				<!-- `codeHtml` is HTML-by-construction: highlight path
					 goes through `highlightCode`, which escapes every
					 text slice inside `highlightTree` before wrapping
					 it in `<span class="tok-…">`; fallback path goes
					 through our `escapeHtml`. There is no branch where
					 untrusted markup reaches `{@html}`. -->
				<pre class="wf-code"><code class="cm-code">{@html codeHtml}</code></pre>
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
	}
	.wf-verb {
		flex: 0 0 auto;
		text-transform: uppercase;
		letter-spacing: 0.06em;
		font-size: 10px;
		color: var(--m-fg-subtle);
	}
	.wf-path {
		flex: 1 1 auto;
		min-width: 0;
		font-family: var(--m-font-mono, ui-monospace, monospace);
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.wf-meta {
		flex: 0 0 auto;
		color: var(--m-fg-subtle);
		font-variant-numeric: tabular-nums;
	}
	.wf-body {
		display: flex;
		align-items: stretch;
		background: var(--m-bg);
		border-radius: 4px;
		max-height: 360px;
		overflow: auto;
		font-family: var(--m-font-mono, ui-monospace, monospace);
		font-size: 11px;
		line-height: 1.4;
	}
	.wf-line-numbers {
		position: sticky;
		left: 0;
		flex: 0 0 auto;
		margin: 0;
		padding: 6px 6px 6px 8px;
		color: var(--m-fg-subtle);
		background: var(--m-bg);
		text-align: right;
		user-select: none;
		white-space: pre;
		font-variant-numeric: tabular-nums;
	}
	.wf-code {
		flex: 1 1 auto;
		min-width: 0;
		margin: 0;
		padding: 6px 8px;
		white-space: pre;
	}
	.wf-code :global(.cm-code) {
		display: block;
	}
</style>
