<script lang="ts">
	// Lightweight Source Control panel that lives in the sidebar
	// directly under the folder bars. Affordances today:
	//
	//   1. Branch label (or short HEAD SHA in the detached-HEAD
	//      case) plus icon buttons for `git pull` and `git push`.
	//   2. Commit-message input. Plain Enter inserts a newline so
	//      multi-line bodies compose naturally; Ctrl/Cmd+Enter
	//      commits every working-tree change (`git add -A && git
	//      commit -m`). The textarea grows up to a small cap.
	//   3. Amend toggle. When on, the next commit is `git commit
	//      --amend` instead of a fresh commit; an empty message
	//      in that mode falls through to `--no-edit` (keep the
	//      previous subject, just absorb the staged changes).
	//
	// The "stage everything then commit" flow is the simplest
	// affordance that matches the user's "commit current changes"
	// gesture — VSCode's per-file staging UI is a later-phase
	// enhancement (Phase 5's full SCM panel). Errors from git
	// (no identity, nothing to commit, push/pull failures, etc.)
	// surface as a flash toast and the input stays focused for
	// retry.

	import { tick } from 'svelte';
	import { workspace } from '../state.svelte';
	import RevertIcon from './icons/RevertIcon.svelte';

	const branch = $derived(workspace.gitBranch);
	const branchLabel = $derived.by(() => {
		if (branch.name !== null) {
			return branch.name;
		}
		if (branch.headShortSha !== null) {
			// Detached HEAD: surface the SHA so the user knows
			// where they are. Parens distinguish "this is a hash,
			// not a branch name".
			return `(${branch.headShortSha})`;
		}
		return null;
	});

	let message = $state('');
	let amend = $state(false);
	let busy = $state(false);
	let textarea: HTMLTextAreaElement | undefined = $state();

	const changeCount = $derived(workspace.scmChangeCount);

	// Amend-with-empty-message is valid (preserve previous
	// subject); fresh commits still need a message. Push and pull
	// buttons just need "not currently busy".
	const canCommit = $derived(!busy && (amend || message.trim().length > 0));

	// Smart sync button label. Falls back to `null` when there's
	// nothing to do (no commits to push or pull) — the button is
	// hidden in that case. Order is "pull then push", echoing
	// VSCode's Source Control sync semantics.
	const syncLabel = $derived.by(() => {
		const a = branch.ahead;
		const b = branch.behind;
		if (a > 0 && b > 0) {
			return `Sync changes  ↓${b}  ↑${a}`;
		}
		if (a > 0) {
			return `Push ${a} commit${a === 1 ? '' : 's'}  ↑`;
		}
		if (b > 0) {
			return `Pull ${b} commit${b === 1 ? '' : 's'}  ↓`;
		}
		return null;
	});

	async function commit() {
		if (!canCommit) {
			return;
		}
		busy = true;
		try {
			const ok = await workspace.commitChanges(message, amend);
			if (ok) {
				message = '';
				amend = false;
				await tick();
				autoSize();
			}
		} finally {
			busy = false;
			textarea?.focus();
		}
	}

	/**
	 * Single-button sync: pulls first if behind, then pushes if
	 * the local branch was (or still is, after the pull) ahead.
	 * If pull fails we bail early so the user doesn't end up with
	 * a non-fast-forward push attempt on top of an unresolved
	 * merge / conflict / dirty-tree situation.
	 */
	async function sync() {
		if (busy) {
			return;
		}
		const initialAhead = branch.ahead;
		const initialBehind = branch.behind;
		if (initialAhead === 0 && initialBehind === 0) {
			return;
		}
		busy = true;
		try {
			if (initialBehind > 0) {
				const ok = await workspace.pullChanges();
				if (!ok) {
					return;
				}
			}
			if (initialAhead > 0) {
				await workspace.pushChanges();
			}
		} finally {
			busy = false;
		}
	}

	/**
	 * "Revert all changes" — discards every non-ignored entry in
	 * `gitStatusEntries`. Tracked-changed paths route through
	 * `git restore --source=HEAD --staged --worktree`; untracked
	 * paths route to the OS trash. `discardPaths` already wraps
	 * the work in a confirm dialog, so this is just "build the
	 * list and hand it over".
	 */
	async function revertAll() {
		if (busy) {
			return;
		}
		const paths = workspace.gitStatusEntries.filter((e) => e.status !== 'ignored').map((e) => e.path);
		if (paths.length === 0) {
			return;
		}
		busy = true;
		try {
			await workspace.discardPaths(paths);
		} finally {
			busy = false;
		}
	}

	// Ctrl/Cmd+Enter commits; plain Enter falls through to the
	// browser's default and inserts a newline (so a multi-line
	// commit body composes naturally). Esc clears the draft so a
	// quick "ah, never mind" doesn't leave the buffer dirty across
	// panel re-renders.
	function onKeydown(event: KeyboardEvent) {
		if (event.key === 'Enter' && (event.ctrlKey || event.metaKey) && !event.isComposing) {
			event.preventDefault();
			void commit();
			return;
		}
		if (event.key === 'Escape' && message.length > 0) {
			event.preventDefault();
			message = '';
			void tick().then(autoSize);
		}
	}

	// Manual autoresize: collapse with `height: auto`, then set
	// height to the post-collapse `scrollHeight`. The CSS
	// `max-height` caps multi-line bodies so they don't push the
	// file tree off-screen. We toggle `overflow-y` based on
	// whether we hit the cap because a permanent
	// `overflow-y: auto` shows a scrollbar in the rare 1-2px
	// rounding gap between `scrollHeight` and the rendered box —
	// content fits, scrollbar appears anyway, looks broken. Hidden
	// by default → switch to `auto` only when the content can't
	// actually fit.
	function autoSize() {
		const el = textarea;
		if (!el) {
			return;
		}
		el.style.height = 'auto';
		const cap = parseFloat(getComputedStyle(el).maxHeight);
		const next = el.scrollHeight;
		if (Number.isFinite(cap) && next > cap) {
			el.style.height = `${cap}px`;
			el.style.overflowY = 'auto';
			return;
		}
		el.style.height = `${next}px`;
		el.style.overflowY = 'hidden';
	}

	// Re-run autosize whenever the message string changes (clearing
	// the draft after a commit needs the textarea to shrink back).
	$effect(() => {
		void message;
		autoSize();
	});
</script>

<section class="scm" aria-label="Source control">
	{#if branchLabel !== null}
		<div class="header">
			<div class="branch" title={branch.name === null ? 'Detached HEAD' : `Branch: ${branch.name}`}>
				<span class="branch-icon" aria-hidden="true">⎇</span>
				<span class="branch-name">{branchLabel}</span>
			</div>
			<div class="actions">
				{#if changeCount > 0}
					<button
						type="button"
						class="icon-btn danger"
						title="Revert all changes"
						aria-label="Revert all changes"
						disabled={busy}
						onclick={revertAll}
					>
						<RevertIcon />
					</button>
				{/if}
				{#if changeCount > 0 || workspace.scmFilterOn}
					<button
						type="button"
						class="changes-badge"
						class:active={workspace.scmFilterOn}
						title={workspace.scmFilterOn
							? `${changeCount} change${changeCount === 1 ? '' : 's'} (click to show all files)`
							: `${changeCount} change${changeCount === 1 ? '' : 's'} (click to filter to changes only)`}
						aria-label={workspace.scmFilterOn
							? `Showing ${changeCount} changes — click to show all files`
							: `${changeCount} changes — click to filter`}
						aria-pressed={workspace.scmFilterOn}
						onclick={() => workspace.toggleScmFilter()}
					>
						{changeCount}
					</button>
				{/if}
			</div>
		</div>
	{/if}
	<div class="composer">
		<textarea
			bind:this={textarea}
			bind:value={message}
			class="input"
			rows="1"
			placeholder={amend ? 'Amend message (leave empty to keep)' : 'Commit message'}
			aria-label="Commit message"
			disabled={busy}
			onkeydown={onKeydown}
			oninput={autoSize}
		></textarea>
		<button
			type="button"
			class="amend-inset"
			class:active={amend}
			title={amend ? 'Amend HEAD on next commit (click to disable)' : 'Toggle amend mode'}
			aria-label={amend ? 'Amend mode on' : 'Toggle amend mode'}
			aria-pressed={amend}
			disabled={busy}
			onclick={() => (amend = !amend)}
		>
			<span aria-hidden="true">✎</span>
		</button>
	</div>
	{#if syncLabel !== null}
		<button type="button" class="sync-btn" disabled={busy} onclick={sync}>
			{syncLabel}
		</button>
	{/if}
</section>

<style>
	.scm {
		display: flex;
		flex-direction: column;
		gap: 4px;
		padding: 6px 8px 8px;
		border-bottom: 1px solid var(--m-border);
		flex-shrink: 0;
	}
	.header {
		display: flex;
		align-items: center;
		gap: 6px;
		color: var(--m-fg-muted);
		font-size: 12px;
		min-width: 0;
	}
	.branch {
		flex: 1;
		min-width: 0;
		display: flex;
		align-items: center;
		gap: 6px;
	}
	.branch-icon {
		flex-shrink: 0;
		font-size: 12px;
		line-height: 1;
		opacity: 0.8;
	}
	.branch-name {
		flex: 1;
		min-width: 0;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
		font-family: var(--m-font-mono, monospace);
	}
	.actions {
		display: flex;
		align-items: center;
		gap: 2px;
		flex-shrink: 0;
	}
	.icon-btn {
		appearance: none;
		display: inline-flex;
		align-items: center;
		justify-content: center;
		width: 22px;
		height: 22px;
		/* Drop the user-agent button padding so the 22×22 box is
		   actually 22×22 of content. Without this the inner SVG
		   (a flex item with default `flex-shrink: 1`) collapses to
		   the leftover content area and renders as a sliver. */
		padding: 0;
		border: none;
		background: transparent;
		color: var(--m-fg-muted);
		cursor: pointer;
		font: inherit;
		font-size: 14px;
		line-height: 1;
		border-radius: 4px;
		flex-shrink: 0;
	}
	.icon-btn:hover:not(:disabled) {
		background: var(--m-bg-2);
		color: var(--m-fg);
	}
	.icon-btn:focus-visible {
		outline: 1px solid var(--m-accent);
		outline-offset: -1px;
	}
	.icon-btn:disabled {
		opacity: 0.4;
		cursor: not-allowed;
	}
	/* Destructive icon — revert / discard / delete. Hover reveals
	   the danger color so the user gets a "this is destructive"
	   warning before clicking. The neutral resting colour avoids
	   permanently lighting up the panel like a fire-alarm; the
	   confirm dialog inside `discardPaths` is the actual safety
	   net. */
	.icon-btn.danger:hover:not(:disabled) {
		background: color-mix(in srgb, var(--m-danger) 14%, transparent);
		color: var(--m-danger);
	}
	/* Composer wrapper hosts the textarea plus the inset amend
	   icon — chat-composer pattern. The textarea stretches the
	   full container; the icon button is absolutely positioned in
	   the bottom-right and the textarea reserves padding-right so
	   wrapping text doesn't run under it. */
	.composer {
		position: relative;
	}
	.input {
		appearance: none;
		display: block;
		width: 100%;
		box-sizing: border-box;
		min-height: 24px;
		max-height: 240px;
		padding: 4px 28px 4px 6px;
		border: 1px solid var(--m-border);
		border-radius: 4px;
		background: var(--m-bg-1);
		color: var(--m-fg);
		font: inherit;
		font-size: 13px;
		line-height: 1.4;
		resize: none;
		/* `auto` toggled in JS only when scrollHeight exceeds
		   max-height — see `autoSize`. The off-by-one rendering
		   gap between scrollHeight and the actual textarea box is
		   what was painting a phantom scrollbar with `auto` left
		   on permanently. */
		overflow-y: hidden;
	}
	.input::placeholder {
		color: var(--m-fg-subtle);
	}
	.input:focus-visible {
		outline: 1px solid var(--m-accent);
		outline-offset: 0;
		border-color: transparent;
	}
	.input:disabled {
		opacity: 0.6;
	}
	/* Inset amend toggle: a small square button sitting in the
	   bottom-right of the composer. Off → muted ghost. On → accent
	   ring matching the rest of the panel's "this control is
	   driving" vocabulary. */
	.amend-inset {
		appearance: none;
		position: absolute;
		right: 4px;
		bottom: 4px;
		display: inline-flex;
		align-items: center;
		justify-content: center;
		width: 20px;
		height: 20px;
		padding: 0;
		border: 1px solid transparent;
		border-radius: 4px;
		background: transparent;
		color: var(--m-fg-muted);
		font: inherit;
		font-size: 12px;
		line-height: 1;
		cursor: pointer;
	}
	.amend-inset:hover:not(:disabled) {
		background: var(--m-bg-2);
		color: var(--m-fg);
	}
	.amend-inset:focus-visible {
		outline: 1px solid var(--m-accent);
		outline-offset: -1px;
	}
	.amend-inset.active {
		background: var(--m-bg-overlay);
		border-color: var(--m-accent);
		color: var(--m-fg);
	}
	.amend-inset:disabled {
		opacity: 0.4;
		cursor: not-allowed;
	}
	/* Pill badge in the header next to the branch name. Always
	   carries the change count; visible iff the user has changes
	   *or* the filter is on (so a count of 0 with the filter
	   active still surfaces the toggle for "go back to all"). The
	   accent fill makes it the loudest control on the panel —
	   matches the user's "more obvious color" ask. The active
	   state inverts to a hollow ring so the toggle reads as
	   "currently driving the tree" without changing colour
	   weight. */
	.changes-badge {
		appearance: none;
		display: inline-flex;
		align-items: center;
		justify-content: center;
		min-width: 20px;
		height: 20px;
		padding: 0 6px;
		border: 1px solid var(--m-accent);
		border-radius: 999px;
		background: var(--m-accent);
		/* `--m-bg` flips between near-black (dark theme) and
		   near-white (light theme), so it always contrasts well
		   against the accent fill — saves us inventing a new
		   token just for this badge. */
		color: var(--m-bg);
		font: inherit;
		font-size: 11px;
		font-weight: 600;
		line-height: 1;
		cursor: pointer;
		font-variant-numeric: tabular-nums;
		flex-shrink: 0;
	}
	.changes-badge:hover {
		filter: brightness(1.1);
	}
	.changes-badge:focus-visible {
		outline: 2px solid var(--m-accent);
		outline-offset: 2px;
	}
	.changes-badge.active {
		background: transparent;
		color: var(--m-accent);
	}
	/* Full-width sync button beneath the composer — the unified
	   alternative to separate push/pull icons. Hidden when the
	   branch has nothing to push or pull (`syncLabel === null`).
	   Accent fill matches the badge's "this is the loud control
	   right now" vocabulary; the label rewrites itself based on
	   ahead/behind state ("Sync changes ↓N ↑M" / "Push N commits"
	   / "Pull N commits"). */
	.sync-btn {
		appearance: none;
		display: block;
		width: 100%;
		min-height: 26px;
		padding: 4px 8px;
		border: 1px solid var(--m-accent);
		border-radius: 4px;
		background: var(--m-accent);
		color: var(--m-bg);
		font: inherit;
		font-size: 12px;
		font-weight: 600;
		line-height: 1.2;
		cursor: pointer;
		font-variant-numeric: tabular-nums;
	}
	.sync-btn:hover:not(:disabled) {
		filter: brightness(1.1);
	}
	.sync-btn:focus-visible {
		outline: 2px solid var(--m-accent);
		outline-offset: 2px;
	}
	.sync-btn:disabled {
		opacity: 0.5;
		cursor: not-allowed;
	}
</style>
