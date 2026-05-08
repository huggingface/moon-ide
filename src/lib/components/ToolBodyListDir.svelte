<script lang="ts">
	import { fmtJson, openToolPath } from './toolBodyHelpers';

	interface Props {
		args: unknown;
		result: unknown;
		hasResult: boolean;
	}

	let { args, result, hasResult }: Props = $props();

	/** Match `crates/moon-coder/src/tools.rs`'s `ListDirArgs`. The
	 *  default-`.` fallback matches the Rust side so a missing
	 *  `path` still renders a sensible header. */
	function parseArgs(a: unknown): { path: string } | null {
		if (typeof a !== 'object' || a === null) {
			return { path: '.' };
		}
		const o = a as { path?: unknown };
		if (typeof o.path === 'string') {
			return { path: o.path };
		}
		return { path: '.' };
	}

	type EntryKind = 'dir' | 'file' | 'link' | 'other';
	type Entry = { kind: EntryKind; name: string };

	/** Match the `json!` block in `tools.rs::list_dir`. The
	 *  `entries` field is a flat string of `<kind> <name>\n` rows
	 *  where `<kind>` is one of `dir `, `file`, `link`, `?   ` —
	 *  see `list_dir` for the source-of-truth mapping. */
	function parseResult(r: unknown): { path: string | null; entries: Entry[]; count: number | null } | null {
		if (typeof r !== 'object' || r === null) {
			return null;
		}
		const o = r as Record<string, unknown>;
		if (typeof o.entries !== 'string') {
			return null;
		}
		const trimmed = o.entries.endsWith('\n') ? o.entries.slice(0, -1) : o.entries;
		const entries: Entry[] = [];
		if (trimmed.length > 0) {
			for (const row of trimmed.split('\n')) {
				// Rust pushes `<kind> ` (kind padded to 4 chars) +
				// space + name. Anything that doesn't match the
				// pattern is preserved as an `other` entry rather
				// than dropped, so the user still sees something.
				const m = /^(dir |file|link|\?\s*)\s(.*)$/.exec(row);
				if (m === null) {
					entries.push({ kind: 'other', name: row });
					continue;
				}
				const kindRaw = (m[1] ?? '').trim();
				const name = m[2] ?? '';
				let kind: EntryKind = 'other';
				if (kindRaw === 'dir') {
					kind = 'dir';
				} else if (kindRaw === 'file') {
					kind = 'file';
				} else if (kindRaw === 'link') {
					kind = 'link';
				}
				entries.push({ kind, name });
			}
		}
		return {
			path: typeof o.path === 'string' ? o.path : null,
			entries,
			count: typeof o.count === 'number' ? o.count : entries.length,
		};
	}

	const argsP = $derived(parseArgs(args));
	const resultP = $derived(hasResult ? parseResult(result) : null);
	const parseable = $derived(argsP !== null || resultP !== null);
	const path = $derived(resultP?.path ?? argsP?.path ?? null);

	/** Build the workspace-relative path for an entry by joining
	 *  the listing's `path` with the entry's name. Treats `.` as
	 *  an empty prefix so `list_dir(".")` produces bare names
	 *  rather than `./foo`, which `openFile` accepts but isn't
	 *  what the rest of the IDE shows. */
	function entryFullPath(name: string): string {
		if (path === null || path === '' || path === '.') {
			return name;
		}
		const base = path.endsWith('/') ? path.slice(0, -1) : path;
		return `${base}/${name}`;
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
	<div class="ld-block">
		<header class="ld-header">
			{#if path !== null}
				<!-- Listing path stays plain text. The IDE doesn't
					 currently expose a "reveal directory in tree"
					 action, so a clickable directory header would
					 be either a noop or an unexpected `openFile`
					 call — neither is a good affordance. -->
				<span class="ld-path">{path}</span>
			{/if}
			{#if resultP !== null}
				<span class="ld-meta">
					{resultP.count ?? resultP.entries.length} entr{(resultP.count ?? resultP.entries.length) === 1 ? 'y' : 'ies'}
				</span>
			{/if}
		</header>
		{#if resultP !== null}
			{#if resultP.entries.length === 0}
				<div class="ld-empty">empty</div>
			{:else}
				<!-- Compact two-column grid: kind glyph on the left,
					 name on the right. Folders get a trailing `/` so
					 a glance at the column lights up the directory
					 structure even if the kind glyph is too small to
					 read; symlinks pick up `→` for the same reason. -->
				<div class="ld-entries">
					{#each resultP.entries as e, idx (idx)}
						<div class="ld-entry" class:dir={e.kind === 'dir'} class:link={e.kind === 'link'}>
							<span class="ld-kind" aria-hidden="true">
								{#if e.kind === 'dir'}
									▾
								{:else if e.kind === 'link'}
									↪
								{:else if e.kind === 'file'}
									·
								{:else}
									?
								{/if}
							</span>
							{#if e.kind === 'file' || e.kind === 'link'}
								<!-- Files are clickable; symlinks too — they
									 typically resolve to a file the user
									 wants to open. Directories stay plain
									 text because we don't currently have a
									 "navigate file tree to" command, and a
									 noop click would be misleading. -->
								<button
									type="button"
									class="ld-name tool-link"
									title={`Open ${entryFullPath(e.name)}`}
									onclick={() => void openToolPath(entryFullPath(e.name))}
								>
									{e.name}
								</button>
							{:else}
								<span class="ld-name"
									>{e.name}{#if e.kind === 'dir'}/{/if}</span
								>
							{/if}
						</div>
					{/each}
				</div>
			{/if}
		{/if}
	</div>
{/if}

<style>
	.ld-block {
		display: flex;
		flex-direction: column;
		gap: 4px;
		margin-top: 4px;
	}
	.ld-header {
		display: flex;
		gap: 8px;
		align-items: baseline;
		font-size: 11px;
	}
	.ld-path {
		flex: 1 1 auto;
		min-width: 0;
		font-family: var(--m-font-mono, ui-monospace, monospace);
		color: var(--m-fg);
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.ld-meta {
		flex: 0 0 auto;
		color: var(--m-fg-subtle);
	}
	.ld-empty {
		font-size: 11px;
		color: var(--m-fg-subtle);
		font-style: italic;
		padding: 6px 8px;
		background: var(--m-bg);
		border-radius: 4px;
	}
	.ld-entries {
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
	.ld-entry {
		display: flex;
		gap: 8px;
		padding: 1px 8px;
	}
	.ld-kind {
		flex: 0 0 1.2em;
		text-align: center;
		color: var(--m-fg-subtle);
		user-select: none;
	}
	/* Layout / typography only — no `color` declaration here so
	   the per-kind colour rules below win cleanly. Files render
	   as `<button class="ld-name tool-link">` and pick up the
	   global accent; directories stay spans with the explicit
	   `.dir .ld-name` accent rule below. */
	.ld-name {
		flex: 1 1 auto;
		min-width: 0;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.ld-entry.dir .ld-kind {
		color: var(--m-accent, var(--m-fg));
	}
	.ld-entry.dir .ld-name {
		color: var(--m-accent, var(--m-fg));
	}
	.ld-entry.link .ld-kind {
		color: var(--m-fg-subtle);
	}
	.ld-entry.link .ld-name {
		font-style: italic;
	}
</style>
