<script lang="ts">
	import { fmtJson, openToolPath, parseToolError } from './toolBodyHelpers';
	import ToolReapplyMenu from './ToolReapplyMenu.svelte';

	interface Props {
		args: unknown;
		result: unknown;
		hasResult: boolean;
		/** Tool-call id, for the "re-apply to disk" recovery menu. */
		callId: string;
	}

	let { args, result, hasResult, callId }: Props = $props();

	/** Match `crates/moon-coder/src/tools.rs`'s `EditFileArgs`. */
	function parseArgs(a: unknown): { path: string; find: string; replace: string; occurrence: number | null } | null {
		if (typeof a !== 'object' || a === null) {
			return null;
		}
		const o = a as {
			path?: unknown;
			file_path?: unknown;
			file?: unknown;
			find?: unknown;
			replace?: unknown;
			occurrence?: unknown;
		};
		const pathRaw = o.path ?? o.file_path ?? o.file;
		if (typeof pathRaw !== 'string' || typeof o.find !== 'string' || typeof o.replace !== 'string') {
			return null;
		}
		return {
			path: pathRaw,
			find: o.find,
			replace: o.replace,
			occurrence: typeof o.occurrence === 'number' ? o.occurrence : null,
		};
	}

	function parseResult(
		r: unknown,
	): { path: string | null; occurrence: number | null; totalMatches: number | null } | null {
		if (typeof r !== 'object' || r === null) {
			return null;
		}
		const o = r as Record<string, unknown>;
		// Heuristic: a real edit_file success has `total_matches`.
		if (typeof o.total_matches !== 'number' && typeof o.bytes_written !== 'number') {
			return null;
		}
		return {
			path: typeof o.path === 'string' ? o.path : null,
			occurrence: typeof o.occurrence === 'number' ? o.occurrence : null,
			totalMatches: typeof o.total_matches === 'number' ? o.total_matches : null,
		};
	}

	/** Strip a single trailing newline so the diff blocks don't
	 *  show a phantom empty trailing line. The model frequently
	 *  ends `find` / `replace` strings with `\n` to anchor on a
	 *  full-line match. */
	function stripTrailingNl(s: string): string {
		return s.endsWith('\n') ? s.slice(0, -1) : s;
	}

	const argsP = $derived(parseArgs(args));
	const resultP = $derived(hasResult ? parseResult(result) : null);
	// Tool-error envelope from the coder runner: `{ "error": "<msg>" }`
	// with `is_error: true`. We surface the message inline above the
	// diff (when `find` / `replace` are still parseable) so the user
	// sees both the attempted edit and why it failed (`find` matched
	// 3 times, file is binary, occurrence out of range, …) without
	// expanding the generic JSON fallback.
	const errorMsg = $derived(hasResult ? parseToolError(result) : null);
	const parseable = $derived(argsP !== null || resultP !== null || errorMsg !== null);
	const path = $derived(resultP?.path ?? argsP?.path ?? null);
	const findText = $derived(argsP !== null ? stripTrailingNl(argsP.find) : '');
	const replaceText = $derived(argsP !== null ? stripTrailingNl(argsP.replace) : '');
	const occurrenceLabel = $derived.by(() => {
		const occ = resultP?.occurrence ?? argsP?.occurrence ?? null;
		const total = resultP?.totalMatches ?? null;
		if (occ === null && total === null) {
			return null;
		}
		if (occ !== null && total !== null) {
			return `occurrence ${occ} of ${total}`;
		}
		if (occ !== null) {
			return `occurrence ${occ}`;
		}
		return `${total} match${total === 1 ? '' : 'es'}`;
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
	<div class="ef-block">
		<header class="ef-header">
			<span class="ef-verb" class:err={errorMsg !== null}>
				{#if errorMsg !== null}
					failed
				{:else if resultP !== null}
					edited
				{:else}
					editing
				{/if}
			</span>
			{#if path !== null}
				<button type="button" class="ef-path tool-link" title={`Open ${path}`} onclick={() => void openToolPath(path)}>
					{path}
				</button>
			{/if}
			{#if occurrenceLabel !== null}
				<span class="ef-meta">{occurrenceLabel}</span>
			{/if}
			{#if path !== null}
				<ToolReapplyMenu {callId} />
			{/if}
		</header>
		{#if errorMsg !== null}
			<!-- Inline error block. The runner emits the same string
				 verbatim back to the model (so the model can recover
				 / retry); rendering it here means the user sees the
				 same signal the model just did, which keeps mental
				 model parity. The diff below still shows what was
				 attempted — useful when the failure is a multi-match
				 ("find matched 4 times in …") and the user wants to
				 see exactly which `find` was ambiguous. -->
			<div class="ef-error" role="alert">{errorMsg}</div>
		{/if}
		{#if argsP !== null}
			<!-- Unified-diff view: a `-` block for the searched
				 string and a `+` block for the replacement. We
				 deliberately don't run them through the syntax
				 highlighter — the model often edits partial
				 expressions (a function signature, a single line
				 inside a multi-line string), and partial-grammar
				 colouring routinely mis-tokenises those. The
				 diff colours alone (red removed / green added)
				 carry the visual signal; monospace + escaped text
				 carries the content. -->
			<div class="ef-diff">
				<div class="ef-side ef-removed">
					<span class="ef-marker" aria-hidden="true">-</span>
					<pre class="ef-text">{findText}</pre>
				</div>
				<div class="ef-side ef-added">
					<span class="ef-marker" aria-hidden="true">+</span>
					<pre class="ef-text">{replaceText}</pre>
				</div>
			</div>
		{/if}
	</div>
{/if}

<style>
	.ef-block {
		display: flex;
		flex-direction: column;
		gap: 4px;
		margin-top: 4px;
	}
	.ef-header {
		display: flex;
		gap: 8px;
		align-items: baseline;
		font-size: 11px;
	}
	.ef-verb {
		flex: 0 0 auto;
		text-transform: uppercase;
		letter-spacing: 0.06em;
		font-size: 10px;
		color: var(--m-fg-subtle);
	}
	.ef-verb.err {
		color: var(--m-danger);
	}
	.ef-error {
		font-family: var(--m-font-mono, ui-monospace, monospace);
		font-size: 11px;
		line-height: 1.4;
		padding: 6px 8px;
		border-radius: 4px;
		background: color-mix(in srgb, var(--m-danger) 14%, transparent);
		color: var(--m-danger);
		white-space: pre-wrap;
		word-break: break-word;
	}
	.ef-path {
		flex: 1 1 auto;
		min-width: 0;
		font-family: var(--m-font-mono, ui-monospace, monospace);
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.ef-meta {
		flex: 0 0 auto;
		color: var(--m-fg-subtle);
	}
	.ef-diff {
		display: flex;
		flex-direction: column;
		gap: 2px;
		font-family: var(--m-font-mono, ui-monospace, monospace);
		font-size: 11px;
		line-height: 1.4;
	}
	.ef-side {
		display: flex;
		gap: 6px;
		padding: 6px 8px;
		border-radius: 4px;
		max-height: 240px;
		overflow: auto;
	}
	.ef-marker {
		flex: 0 0 1ch;
		user-select: none;
		font-weight: 600;
	}
	.ef-text {
		flex: 1 1 auto;
		min-width: 0;
		margin: 0;
		white-space: pre-wrap;
		word-break: break-word;
	}
	/* Tinted backgrounds rather than full danger / success colours
	   so a 100-line replace block doesn't visually drown out the
	   surrounding transcript. The marker (`-` / `+`) carries the
	   "is this added or removed" signal in saturated colour; the
	   block itself sits on a subtle wash. */
	.ef-removed {
		background: color-mix(in srgb, var(--m-danger) 12%, transparent);
	}
	.ef-removed .ef-marker {
		color: var(--m-danger);
	}
	.ef-added {
		background: color-mix(in srgb, var(--m-success, var(--m-accent)) 12%, transparent);
	}
	.ef-added .ef-marker {
		color: var(--m-success, var(--m-accent));
	}
</style>
