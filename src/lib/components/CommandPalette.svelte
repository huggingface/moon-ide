<script lang="ts">
	import {
		palette,
		commandTitle,
		filterCommands,
		runFileSearch,
		runContentSearch,
		type Command,
	} from '../commands.svelte';
	import { workspace } from '../state.svelte';

	let inputEl: HTMLInputElement | undefined = $state();
	let selected = $state(0);
	let debounceTimer: ReturnType<typeof setTimeout> | null = null;

	// Reactive results based on the current mode + query.
	const commandList: Command[] = $derived.by(() => (palette.mode === 'commands' ? filterCommands(palette.query) : []));

	$effect(() => {
		if (palette.open && inputEl) {
			inputEl.focus();
			inputEl.select();
		}
	});

	// Search modes hit the backend; debounce so we don't spam Rust.
	// Flipping any of the search-mode toggles (case, whole-word,
	// regex, include filter) refires the search against the current
	// query — same debounce window as a keystroke, so a rapid
	// "type word, then click `Aa`" sequence collapses to one round-
	// trip rather than two.
	$effect(() => {
		if (!palette.open) {
			return;
		}
		const q = palette.query;
		const mode = palette.mode;
		// Read these here so Svelte tracks them as dependencies of
		// this effect; the actual values are picked up off
		// `palette.*` inside `runContentSearch` when the timer
		// fires. The `void` discards keep oxlint happy without
		// adding a noisy intermediate.
		void palette.searchCaseSensitive;
		void palette.searchWholeWord;
		void palette.searchRegex;
		void palette.searchInclude;
		if (mode === 'commands') {
			return;
		}

		if (debounceTimer) {
			clearTimeout(debounceTimer);
		}
		debounceTimer = setTimeout(
			() => {
				if (mode === 'files') {
					void runFileSearch(q);
				}
				if (mode === 'search') {
					void runContentSearch(q);
				}
			},
			mode === 'search' ? 200 : 50,
		);
	});

	$effect(() => {
		palette.query;
		palette.mode;
		selected = 0;
	});

	const totalRows: number = $derived.by(() => {
		if (palette.mode === 'commands') {
			return commandList.length;
		}
		if (palette.mode === 'files') {
			return palette.fileResults.length;
		}
		return palette.contentResults.length;
	});

	function placeholder() {
		if (palette.mode === 'commands') {
			return 'Type a command…';
		}
		if (palette.mode === 'files') {
			return 'Search files by name…';
		}
		return 'Search in files…';
	}

	function activate(index: number) {
		if (palette.mode === 'commands') {
			const cmd = commandList[index];
			if (!cmd) {
				return;
			}
			palette.hide();
			void cmd.run();
			return;
		}
		if (palette.mode === 'files') {
			const hit = palette.fileResults[index];
			if (!hit) {
				return;
			}
			palette.hide();
			void workspace.openFile(hit.path);
			return;
		}
		const hit = palette.contentResults[index];
		if (!hit) {
			return;
		}
		palette.hide();
		// Search hits return 1-indexed `line` / `column` (grep-searcher
		// convention); `jumpTo` consumes 0-indexed LSP positions. The
		// `character` is a UTF-8 byte offset on the line — exact for
		// ASCII content, off by a few units when non-ASCII precedes the
		// match. Acceptable until we wire a proper byte→UTF-16 mapper;
		// landing on the wrong column on the right line beats not
		// landing at all (the prior behavior).
		void workspace.jumpTo(hit.path, {
			line: Math.max(0, hit.line - 1),
			character: Math.max(0, hit.column - 1),
		});
	}

	function onKey(event: KeyboardEvent) {
		if (event.key === 'Escape') {
			event.preventDefault();
			palette.hide();
			return;
		}
		if (event.key === 'ArrowDown') {
			event.preventDefault();
			selected = Math.min(selected + 1, Math.max(0, totalRows - 1));
			return;
		}
		if (event.key === 'ArrowUp') {
			event.preventDefault();
			selected = Math.max(selected - 1, 0);
			return;
		}
		if (event.key === 'Enter') {
			event.preventDefault();
			activate(selected);
			return;
		}
	}

	function onBackdrop(event: MouseEvent) {
		if (event.target === event.currentTarget) {
			palette.hide();
		}
	}
</script>

{#if palette.open}
	<!-- The backdrop is a click target only (role="presentation"). Key
		 events live on the inner <input>, which has focus while the
		 palette is open. Don't add onkeydown here too — the event would
		 bubble from the input and fire `onKey` twice (Enter would
		 activate the same command twice, ArrowDown/ArrowUp would jump
		 by 2). -->
	<div class="backdrop" role="presentation" onclick={onBackdrop} tabindex="-1">
		<div class="palette" role="dialog" aria-label="Command palette">
			<div class="row">
				<span class="prefix">
					{#if palette.mode === 'commands'}>
					{:else if palette.mode === 'files'}@
					{:else}/
					{/if}
				</span>
				<input
					bind:this={inputEl}
					type="text"
					placeholder={placeholder()}
					value={palette.query}
					oninput={(e) => palette.setQuery(e.currentTarget.value)}
					onkeydown={onKey}
				/>
				{#if palette.mode === 'search'}
					<!-- VS Code-style toggle trio sitting at the end of the
					     search input. Each button is press-and-stay
					     (aria-pressed); flipping any of them refires the
					     search through the effect above. The buttons live
					     on the same row as the input — they're small
					     enough not to crowd the query, and they read as
					     "options for *this* search" rather than as a
					     separate toolbar. -->
					<div class="search-toggles" aria-label="Search options">
						<button
							type="button"
							class="search-toggle"
							class:active={palette.searchCaseSensitive}
							title="Match case (Aa)"
							aria-label="Match case"
							aria-pressed={palette.searchCaseSensitive}
							onclick={() => palette.toggleSearchCaseSensitive()}>Aa</button
						>
						<button
							type="button"
							class="search-toggle"
							class:active={palette.searchWholeWord}
							title="Match whole word"
							aria-label="Match whole word"
							aria-pressed={palette.searchWholeWord}
							onclick={() => palette.toggleSearchWholeWord()}>ab|</button
						>
						<button
							type="button"
							class="search-toggle"
							class:active={palette.searchRegex}
							title="Use regular expression"
							aria-label="Use regular expression"
							aria-pressed={palette.searchRegex}
							onclick={() => palette.toggleSearchRegex()}>.*</button
						>
					</div>
				{/if}
				{#if palette.loading}
					<span class="loading">…</span>
				{/if}
			</div>
			{#if palette.mode === 'search'}
				<!-- Second-row include filter: empty means "search
				     everywhere", a bare path scopes to that subtree
				     (server-side normalises `src/lib` → `src/lib/**`),
				     and globs like `**/*.svelte` pass through verbatim.
				     The placeholder mentions both shapes so users
				     don't have to discover the glob support from
				     trial and error. -->
				<div class="row">
					<span class="prefix sub-prefix" aria-hidden="true">in</span>
					<input
						type="text"
						placeholder="Path or glob (e.g. src/lib or **/*.svelte) — leave blank for entire workspace"
						value={palette.searchInclude}
						oninput={(e) => palette.setSearchInclude(e.currentTarget.value)}
						onkeydown={onKey}
					/>
				</div>
			{/if}
			<ul class="results" role="listbox">
				{#if palette.mode === 'commands'}
					{#each commandList as cmd, i (cmd.id)}
						<!-- svelte-ignore a11y_click_events_have_key_events -->
						<li
							class="result"
							class:selected={i === selected}
							role="option"
							aria-selected={i === selected}
							onmousemove={() => (selected = i)}
							onclick={() => activate(i)}
						>
							<span class="title">{commandTitle(cmd)}</span>
							{#if cmd.shortcut}<span class="shortcut">{cmd.shortcut}</span>{/if}
						</li>
					{/each}
					{#if commandList.length === 0}
						<li class="empty">No commands match.</li>
					{/if}
				{:else if palette.mode === 'files'}
					{#each palette.fileResults as hit, i (hit.path + i)}
						<!-- svelte-ignore a11y_click_events_have_key_events -->
						<li
							class="result"
							class:selected={i === selected}
							role="option"
							aria-selected={i === selected}
							onmousemove={() => (selected = i)}
							onclick={() => activate(i)}
						>
							<span class="title">{hit.path}</span>
						</li>
					{/each}
					{#if !palette.loading && palette.query.trim() !== '' && palette.fileResults.length === 0}
						<li class="empty">No files match.</li>
					{/if}
				{:else}
					{#each palette.contentResults as hit, i (hit.path + ':' + hit.line + ':' + i)}
						<!-- svelte-ignore a11y_click_events_have_key_events -->
						<li
							class="result content-row"
							class:selected={i === selected}
							role="option"
							aria-selected={i === selected}
							onmousemove={() => (selected = i)}
							onclick={() => activate(i)}
						>
							<span class="loc">{hit.path}:{hit.line}</span>
							<code class="line">{hit.line_text}</code>
						</li>
					{/each}
					{#if palette.contentTruncated}
						<li class="empty">More results available — narrow your search.</li>
					{/if}
					{#if !palette.loading && palette.query.trim() !== '' && palette.contentResults.length === 0}
						<li class="empty">No matches.</li>
					{/if}
				{/if}
			</ul>
			<div class="hint">
				<span>↵ open · ↑↓ navigate · Esc close</span>
				<span class="modes">
					<button class:active={palette.mode === 'commands'} onclick={() => palette.show('commands', '')}>Cmds</button>
					<button class:active={palette.mode === 'files'} onclick={() => palette.show('files', '')}>Files</button>
					<button class:active={palette.mode === 'search'} onclick={() => palette.show('search', '')}>Search</button>
				</span>
			</div>
		</div>
	</div>
{/if}

<style>
	.backdrop {
		position: fixed;
		inset: 0;
		background: rgba(0, 0, 0, 0.45);
		z-index: 100;
		display: flex;
		align-items: flex-start;
		justify-content: center;
		padding-top: 80px;
	}
	.palette {
		width: min(640px, 90vw);
		max-height: 60vh;
		background: var(--m-bg-2);
		border: 1px solid var(--m-border-strong);
		border-radius: 8px;
		box-shadow: 0 24px 60px rgba(0, 0, 0, 0.6);
		display: flex;
		flex-direction: column;
		overflow: hidden;
	}
	.row {
		display: flex;
		align-items: center;
		gap: 8px;
		padding: 10px 12px;
		border-bottom: 1px solid var(--m-border);
	}
	.prefix {
		color: var(--m-fg-subtle);
		font-family: var(--m-font-mono);
		font-size: 13px;
		width: 14px;
		text-align: center;
	}
	input {
		flex: 1;
		background: transparent;
		border: none;
		color: var(--m-fg);
		font: inherit;
		outline: none;
	}
	input::placeholder {
		color: var(--m-fg-subtle);
	}
	.loading {
		color: var(--m-fg-subtle);
	}
	/* Compact toggle trio at the trailing edge of the search input.
	   Each button is a single-line glyph (`Aa`, `ab|`, `.*`) so the
	   row stays uncluttered; pressed state mirrors the SCM panel's
	   "active pill" vocabulary — accent fill with the panel bg as
	   the contrasting text — so the cluster reads consistently
	   alongside the rest of the chrome. */
	.search-toggles {
		display: flex;
		align-items: center;
		gap: 2px;
		flex-shrink: 0;
	}
	.search-toggle {
		appearance: none;
		display: inline-flex;
		align-items: center;
		justify-content: center;
		min-width: 24px;
		height: 22px;
		padding: 0 4px;
		border: 1px solid var(--m-border);
		border-radius: 4px;
		background: transparent;
		color: var(--m-fg-muted);
		font: inherit;
		font-family: var(--m-font-mono, monospace);
		font-size: 11px;
		line-height: 1;
		cursor: pointer;
	}
	.search-toggle:hover {
		background: var(--m-bg-3);
		color: var(--m-fg);
	}
	.search-toggle:focus-visible {
		outline: 1px solid var(--m-accent);
		outline-offset: -1px;
	}
	.search-toggle.active {
		background: var(--m-accent);
		border-color: var(--m-accent);
		color: var(--m-bg);
	}
	.search-toggle.active:hover {
		filter: brightness(1.1);
	}
	/* Path-include row sits flush under the query row. `.row`'s own
	   `border-bottom` already provides the visual separator
	   between the two rows, so the sub-row inherits the rest of
	   the `.row` rhythm without overriding anything. `sub-prefix`
	   re-uses the prefix slot as a short "in" label so the row
	   reads as "search [query] in [path]" without needing a
	   second visible field label. */
	.sub-prefix {
		font-family: var(--m-font-mono);
		font-size: 11px;
		text-transform: lowercase;
		opacity: 0.7;
	}
	.results {
		list-style: none;
		margin: 0;
		padding: 4px 0;
		overflow-y: auto;
		flex: 1;
		min-height: 0;
	}
	/* Files/search modes render no <li> while the query is empty (no
	   results yet, and the "No matches" hint is gated on a non-empty
	   query). Without this, the 4px+4px padding leaves an 8px band of
	   dead space between the input divider and the hint divider. */
	.results:empty {
		padding: 0;
	}
	.result {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 12px;
		padding: 6px 14px;
		cursor: pointer;
		color: var(--m-fg-muted);
	}
	.result.selected {
		background: var(--m-accent);
		color: #0d1017;
	}
	.result .title {
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.shortcut {
		font-family: var(--m-font-mono);
		font-size: 11px;
		color: inherit;
		opacity: 0.7;
	}
	.content-row {
		flex-direction: column;
		align-items: flex-start;
		gap: 2px;
	}
	.loc {
		font-family: var(--m-font-mono);
		font-size: 11px;
		opacity: 0.8;
	}
	.line {
		font-family: var(--m-font-mono);
		font-size: 12px;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
		width: 100%;
		color: inherit;
	}
	.empty {
		padding: 8px 14px;
		color: var(--m-fg-subtle);
		font-size: 12px;
	}
	.hint {
		display: flex;
		align-items: center;
		justify-content: space-between;
		padding: 6px 12px;
		border-top: 1px solid var(--m-border);
		font-size: 11px;
		color: var(--m-fg-subtle);
	}
	.modes {
		display: flex;
		gap: 6px;
	}
	.modes button {
		padding: 2px 6px;
		border: 1px solid var(--m-border);
		border-radius: 4px;
		color: var(--m-fg-muted);
		font-size: 11px;
	}
	.modes button.active {
		background: var(--m-bg-3);
		color: var(--m-fg);
	}
</style>
