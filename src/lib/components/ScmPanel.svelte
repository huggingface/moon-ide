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

	// Amend-with-empty-message is valid (preserve previous
	// subject); fresh commits still need a message. Push and pull
	// buttons just need "not currently busy".
	const canCommit = $derived(!busy && (amend || message.trim().length > 0));

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

	async function push() {
		if (busy) {
			return;
		}
		busy = true;
		try {
			await workspace.pushChanges();
		} finally {
			busy = false;
		}
	}

	async function pull() {
		if (busy) {
			return;
		}
		busy = true;
		try {
			await workspace.pullChanges();
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
				<button type="button" class="icon-btn" title="Pull" aria-label="Pull" disabled={busy} onclick={pull}>
					<span aria-hidden="true">↓</span>
				</button>
				<button type="button" class="icon-btn" title="Push" aria-label="Push" disabled={busy} onclick={push}>
					<span aria-hidden="true">↑</span>
				</button>
			</div>
		</div>
	{/if}
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
	<div class="footer">
		<button
			type="button"
			class="amend-toggle"
			class:active={amend}
			title={amend ? 'Amend HEAD on next commit' : 'Toggle amend mode'}
			aria-pressed={amend}
			disabled={busy}
			onclick={() => (amend = !amend)}
		>
			<span class="amend-icon" aria-hidden="true">✎</span>
			<span>Amend</span>
		</button>
	</div>
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
		border: none;
		background: transparent;
		color: var(--m-fg-muted);
		cursor: pointer;
		font: inherit;
		font-size: 14px;
		line-height: 1;
		border-radius: 4px;
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
	.input {
		appearance: none;
		display: block;
		width: 100%;
		box-sizing: border-box;
		min-height: 24px;
		max-height: 240px;
		padding: 4px 6px;
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
	.footer {
		display: flex;
		align-items: center;
	}
	.amend-toggle {
		appearance: none;
		display: inline-flex;
		align-items: center;
		gap: 4px;
		border: 1px solid transparent;
		background: transparent;
		color: var(--m-fg-muted);
		font: inherit;
		font-size: 12px;
		line-height: 1;
		padding: 3px 6px;
		border-radius: 4px;
		cursor: pointer;
	}
	.amend-toggle:hover:not(:disabled) {
		background: var(--m-bg-2);
		color: var(--m-fg);
	}
	.amend-toggle:focus-visible {
		outline: 1px solid var(--m-accent);
		outline-offset: -1px;
	}
	.amend-toggle.active {
		background: var(--m-bg-overlay);
		border-color: var(--m-accent);
		color: var(--m-fg);
	}
	.amend-toggle:disabled {
		opacity: 0.4;
		cursor: not-allowed;
	}
	.amend-icon {
		font-size: 11px;
		line-height: 1;
		opacity: 0.85;
	}
</style>
