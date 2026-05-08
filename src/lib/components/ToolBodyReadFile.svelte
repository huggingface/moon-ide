<script lang="ts">
	import {
		escapeHtml,
		extractInnerHtml,
		fenceLangFromPath,
		fmtJson,
		highlightCode,
		loadHighlighters,
		openToolPath,
	} from './toolBodyHelpers';

	interface Props {
		args: unknown;
		result: unknown;
		hasResult: boolean;
	}

	let { args, result, hasResult }: Props = $props();

	/** Match `crates/moon-coder/src/tools.rs`'s `ReadFileArgs`. */
	function parseArgs(a: unknown): { path: string; startLine: number | null; endLine: number | null } | null {
		if (typeof a !== 'object' || a === null) {
			return null;
		}
		const o = a as { path?: unknown; start_line?: unknown; end_line?: unknown };
		if (typeof o.path !== 'string') {
			return null;
		}
		return {
			path: o.path,
			startLine: typeof o.start_line === 'number' ? o.start_line : null,
			endLine: typeof o.end_line === 'number' ? o.end_line : null,
		};
	}

	/** Match the `json!` block in `tools.rs::read_file`. Heuristic
	 *  shape check: a real read_file result has a string `content`
	 *  field; anything missing it isn't read_file-shaped (an error
	 *  result that returned a plain string, an older trace, etc.)
	 *  and we fall back to the raw-JSON view via `parseable === false`. */
	function parseResult(r: unknown): {
		path: string | null;
		content: string;
		startLine: number | null;
		endLine: number | null;
		totalLines: number | null;
		truncated: boolean;
	} | null {
		if (typeof r !== 'object' || r === null) {
			return null;
		}
		const o = r as Record<string, unknown>;
		if (typeof o.content !== 'string') {
			return null;
		}
		return {
			path: typeof o.path === 'string' ? o.path : null,
			content: o.content,
			startLine: typeof o.start_line === 'number' ? o.start_line : null,
			endLine: typeof o.end_line === 'number' ? o.end_line : null,
			totalLines: typeof o.total_lines === 'number' ? o.total_lines : null,
			truncated: o.truncated === true,
		};
	}

	/** Split the rendered `<line_no>|<text>` payload into a
	 *  parallel `lineNumbers` string (right-aligned, one per line)
	 *  and a `code` string (the file's actual contents). The Rust
	 *  side always uses `writeln!` so the input reliably ends with
	 *  `\n`; we strip exactly that one trailing newline before
	 *  splitting to avoid a phantom empty trailing row. */
	function parseNumberedContent(content: string): { lineNumbers: string; code: string } {
		if (content.length === 0) {
			return { lineNumbers: '', code: '' };
		}
		const trimmed = content.endsWith('\n') ? content.slice(0, -1) : content;
		const lines = trimmed.split('\n');
		const nums: string[] = [];
		const codes: string[] = [];
		for (const line of lines) {
			const idx = line.indexOf('|');
			if (idx < 0) {
				nums.push('');
				codes.push(line);
				continue;
			}
			nums.push(line.slice(0, idx).trim());
			codes.push(line.slice(idx + 1));
		}
		return { lineNumbers: nums.join('\n'), code: codes.join('\n') };
	}

	const argsP = $derived(parseArgs(args));
	const resultP = $derived(hasResult ? parseResult(result) : null);
	const parseable = $derived(argsP !== null || resultP !== null);
	const path = $derived(resultP?.path ?? argsP?.path ?? null);
	const lang = $derived(fenceLangFromPath(path));
	const numbered = $derived(resultP !== null ? parseNumberedContent(resultP.content) : { lineNumbers: '', code: '' });

	// Async-warm the highlighter for `lang` once per language we
	// see. Rendering starts in the unready state so the first
	// frame is plain (escaped) text — once the parser arrives,
	// `parserReady` flips and the `$derived` `codeHtml` recomputes
	// to the highlighted version. The teardown guard avoids a
	// late `loadHighlighters` resolution from updating a row that
	// got destroyed (collapsed and remounted, or scrolled out).
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

	const codeHtml = $derived.by(() => {
		if (numbered.code.length === 0) {
			return '';
		}
		if (lang !== null && parserReady) {
			const wrapped = highlightCode(numbered.code, lang);
			if (wrapped.length > 0) {
				return extractInnerHtml(wrapped);
			}
		}
		return escapeHtml(numbered.code);
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
	<div class="rf-block">
		<header class="rf-header">
			{#if path !== null}
				<!-- Path is a button that opens the file at its
					 starting line (or line 1 when the model didn't
					 ask for a range). Same affordance as a grep hit
					 — `tool-link` styling and `openToolPath` routing. -->
				<button
					type="button"
					class="rf-path tool-link"
					title={`Open ${path}`}
					onclick={() => void openToolPath(path, resultP?.startLine ?? argsP?.startLine ?? null)}
				>
					{path}
				</button>
			{/if}
			{#if resultP !== null}
				<span class="rf-meta">
					lines {resultP.startLine ?? '?'}–{resultP.endLine ?? '?'} of {resultP.totalLines ??
						'?'}{#if resultP.truncated}
						· truncated
					{/if}
				</span>
			{:else if argsP !== null && (argsP.startLine !== null || argsP.endLine !== null)}
				<span class="rf-meta">
					lines {argsP.startLine ?? '?'}–{argsP.endLine ?? '?'}
				</span>
			{/if}
		</header>
		{#if resultP !== null}
			<div class="rf-body">
				<pre class="rf-line-numbers" aria-hidden="true">{numbered.lineNumbers}</pre>
				<!-- `codeHtml` is HTML-by-construction, never raw user
					 input: the highlight path runs the source through
					 `highlightCode`, which escapes every text slice
					 inside `highlightTree` before wrapping it in
					 `<span class="tok-…">` tokens; the fallback path
					 runs the source through our own `escapeHtml`.
					 Both produce strings whose only tags are the
					 highlighter's own `<span>` set — there is no
					 path where untrusted markup reaches `{@html}`. -->
				<pre class="rf-code"><code class="cm-code">{@html codeHtml}</code></pre>
			</div>
		{/if}
	</div>
{/if}

<style>
	.rf-block {
		display: flex;
		flex-direction: column;
		gap: 4px;
		margin-top: 4px;
	}
	.rf-header {
		display: flex;
		gap: 8px;
		align-items: baseline;
		font-size: 11px;
	}
	/* Path is a clickable button (via `.tool-link`); we set
	   layout / typography here only and let the global link
	   class provide the accent colour and hover underline. */
	.rf-path {
		flex: 1 1 auto;
		min-width: 0;
		font-family: var(--m-font-mono, ui-monospace, monospace);
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.rf-meta {
		flex: 0 0 auto;
		color: var(--m-fg-subtle);
	}
	/* Two-column layout: a sticky left column with right-aligned
	   line numbers, a right column with the highlighted code.
	   Both columns share the parent's font-size and line-height,
	   so each line in the numbers column lines up with its
	   counterpart in the code column.

	   `position: sticky` on the numbers column keeps it pinned to
	   the left while the code column scrolls horizontally past
	   it for long lines — which read_file traces hit constantly
	   (minified JSON, deeply indented Rust, etc.). The shared
	   background under both columns means the scrolled code
	   never bleeds through the numbers. */
	.rf-body {
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
	.rf-line-numbers {
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
	.rf-code {
		flex: 1 1 auto;
		min-width: 0;
		margin: 0;
		padding: 6px 8px;
		white-space: pre;
	}
	/* Inherit the editor's `cm-*` token styles. `highlightCode`
	   emits `<span class="tok-keyword">` etc. which our editor
	   theme already paints; reusing the same class set means a
	   read_file body and the live editor agree on colour for the
	   same code, which is exactly the point of routing both
	   surfaces through `@lezer/highlight`'s `classHighlighter`. */
	.rf-code :global(.cm-code) {
		display: block;
	}
</style>
