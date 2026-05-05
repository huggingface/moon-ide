<script lang="ts">
	import { onMount, tick } from 'svelte';
	import { confirm } from '@tauri-apps/plugin-dialog';
	import { coder } from '../coder.svelte';
	import { slack } from '../slack.svelte';
	import { workspace } from '../state.svelte';
	import CoderConnectModal from './CoderConnectModal.svelte';
	import CoderMarkdown from './CoderMarkdown.svelte';
	import CoderThinking from './CoderThinking.svelte';
	import TerminalTargetIcon from './TerminalTargetIcon.svelte';

	let scrollEl: HTMLDivElement | undefined = $state();
	let composer: HTMLTextAreaElement | undefined = $state();

	onMount(() => {
		void coder.refreshStatus();
	});

	// Re-probe `coder_status` when the active folder switches so the
	// host-vs-container indicator pip updates without waiting for the
	// next user action. Reading `workspace.activeFolder?.host` here
	// makes the effect re-run on either path or host change.
	$effect(() => {
		const _kind = workspace.activeFolder?.host ?? null;
		void _kind;
		void coder.refreshStatus();
	});

	// Auto-scroll the transcript when new rows land. Bound to
	// `coder.rows.length` so we don't fire on every text-delta once
	// streaming arrives in 6.1.
	$effect(() => {
		const _trigger = coder.rows.length;
		void _trigger;
		void tick().then(() => {
			if (scrollEl) {
				scrollEl.scrollTop = scrollEl.scrollHeight;
			}
		});
	});

	async function onComposerKey(event: KeyboardEvent) {
		// Enter sends; Shift+Enter inserts a newline. Esc aborts the
		// active turn (matches the panel header's stop button).
		if (event.key === 'Escape' && coder.busy) {
			event.preventDefault();
			await coder.abort();
			return;
		}
		if (event.key === 'Enter' && !event.shiftKey && !event.ctrlKey && !event.metaKey) {
			event.preventDefault();
			await coder.send();
		}
	}

	async function onSignOut() {
		const ok = await confirm('Sign out of Hugging Face?', { title: 'Sign out', kind: 'warning' });
		if (ok) {
			await coder.signOut();
		}
	}

	function fmtArgs(value: unknown): string {
		if (value === null || value === undefined) {
			return '';
		}
		try {
			return JSON.stringify(value, null, 2);
		} catch {
			return String(value);
		}
	}

	async function onNewSession(): Promise<void> {
		await coder.newSession();
		// Land focus in the composer so a fresh session is one
		// keystroke away from being filled in.
		await tick();
		composer?.focus();
	}

	async function onPickSession(id: string): Promise<void> {
		await coder.openSession(id);
		await tick();
		composer?.focus();
	}

	async function onDeleteSession(event: MouseEvent, id: string, title: string): Promise<void> {
		// Stop the click from propagating into the row's "open"
		// button — without this, deleting a session would also
		// open it for a brief moment.
		event.stopPropagation();
		const ok = await confirm(`Delete session "${title || '(untitled)'}"? This cannot be undone.`, {
			title: 'Delete session',
			kind: 'warning',
		});
		if (!ok) {
			return;
		}
		await coder.deleteSession(id);
	}

	const RELATIVE_FORMATTER = new Intl.RelativeTimeFormat(undefined, { numeric: 'auto' });

	function formatRelative(ms: number): string {
		const diff = Date.now() - ms;
		// Coarse buckets — sessions usually span minutes to days,
		// not seconds. Mirrors the chat panel's "2m" / "3h" feel
		// without pulling in date-fns.
		const seconds = Math.round(diff / 1000);
		if (seconds < 60) {
			return RELATIVE_FORMATTER.format(-seconds, 'second');
		}
		const minutes = Math.round(seconds / 60);
		if (minutes < 60) {
			return RELATIVE_FORMATTER.format(-minutes, 'minute');
		}
		const hours = Math.round(minutes / 60);
		if (hours < 24) {
			return RELATIVE_FORMATTER.format(-hours, 'hour');
		}
		const days = Math.round(hours / 24);
		if (days < 30) {
			return RELATIVE_FORMATTER.format(-days, 'day');
		}
		const months = Math.round(days / 30);
		return RELATIVE_FORMATTER.format(-months, 'month');
	}
</script>

<div class="panel" data-region="coder">
	<header class="header">
		<div class="title">
			<span class="label">Coder</span>
			{#if coder.identity}
				<span class="who">{coder.identity.username}</span>
			{/if}
			{#if coder.bashTarget}
				<span
					class="target"
					class:container={coder.bashTarget === 'container'}
					title={coder.bashTarget === 'container'
						? 'bash and shell tools run inside the workspace container'
						: 'bash and shell tools run on the host machine'}
					aria-label={coder.bashTarget === 'container' ? 'shell target: container' : 'shell target: host'}
				>
					<TerminalTargetIcon kind={coder.bashTarget} size={12} />
				</span>
			{/if}
		</div>
		<div class="actions">
			{#if coder.busy}
				<button type="button" class="stop" title="Stop turn (Esc)" onclick={() => coder.abort()}>stop</button>
			{/if}
			<!-- Swap the right-side slot from coder to chat. Same
				 affordance the chat panel has in the other
				 direction. -->
			<button
				type="button"
				class="icon"
				title="Switch to Chat"
				aria-label="Switch to Chat"
				onclick={() => slack.togglePanel()}
			>
				<svg
					viewBox="0 0 16 16"
					width="14"
					height="14"
					fill="none"
					stroke="currentColor"
					stroke-width="1.4"
					stroke-linecap="round"
					stroke-linejoin="round"
					aria-hidden="true"
				>
					<!-- Speech bubble — generic "chat" rather than the
						 trademarked Slack hash; the panel itself is
						 chat regardless of the backend (we may add
						 non-Slack chats later). -->
					<path
						d="M2.5 4a1.5 1.5 0 0 1 1.5-1.5h8A1.5 1.5 0 0 1 13.5 4v5a1.5 1.5 0 0 1-1.5 1.5H6.5L3.5 13v-2.5H4A1.5 1.5 0 0 1 2.5 9z"
					/>
				</svg>
			</button>
			{#if coder.signedIn}
				<button type="button" class="icon" title="Sign out" aria-label="Sign out" onclick={onSignOut}>
					<svg viewBox="0 0 16 16" width="14" height="14" aria-hidden="true">
						<path
							d="M9 1H4a1 1 0 0 0-1 1v12a1 1 0 0 0 1 1h5"
							fill="none"
							stroke="currentColor"
							stroke-width="1.4"
							stroke-linecap="round"
						/>
						<path
							d="M11 5l3 3-3 3M14 8H7"
							fill="none"
							stroke="currentColor"
							stroke-width="1.4"
							stroke-linecap="round"
							stroke-linejoin="round"
						/>
					</svg>
				</button>
			{/if}
		</div>
	</header>

	{#if !coder.signedIn}
		<div class="empty">
			<p class="empty-lede">Sign in with Hugging Face to use the AI coder.</p>
			<button type="button" class="primary" onclick={() => coder.startDeviceFlow()} disabled={coder.startingFlow}>
				{coder.startingFlow ? 'Requesting code…' : 'Sign in with Hugging Face'}
			</button>
			{#if coder.signInError && coder.deviceCode === null}
				<p class="error">{coder.signInError}</p>
			{/if}
		</div>
	{:else if coder.view === 'list'}
		<!-- Sessions list view (mirrors the Slack panel's "← Sessions"
			 affordance). Sticky header has the "+" button; the list
			 itself takes care of empty state. -->
		<div class="sessions">
			<header class="sessions-header">
				<span class="section-title">Sessions</span>
				<div class="header-actions">
					<button type="button" class="icon" onclick={onNewSession} title="New session" aria-label="New session">
						{@render plusIcon()}
					</button>
				</div>
			</header>
			{#if coder.sessions === null}
				<p class="hint">Loading sessions…</p>
			{:else if coder.sessions.length === 0}
				<p class="hint">
					No sessions yet. Click <strong>+</strong> above (or send a prompt) to start a fresh conversation.
				</p>
			{:else}
				<ul class="session-list">
					{#each coder.sessions as session (session.id)}
						<li class="session-row" class:active={coder.activeSession?.id === session.id}>
							<button type="button" class="session-pick" onclick={() => onPickSession(session.id)} title="Open session">
								<div class="session-title">{session.title || '(untitled)'}</div>
								<div class="session-meta">{formatRelative(session.updated_at_ms)}</div>
							</button>
							<button
								type="button"
								class="icon session-delete"
								title="Delete session"
								aria-label="Delete session"
								onclick={(event) => onDeleteSession(event, session.id, session.title)}
							>
								{@render trashIcon()}
							</button>
						</li>
					{/each}
				</ul>
			{/if}
		</div>
	{:else}
		<!-- Sticky in-session header: a small back-to-list affordance,
			 the session title (centre, prominent), and the "+ new"
			 button. Both buttons inherit `.icon`'s muted styling so
			 the title stays the visual focus — this strip is for
			 navigation, not headline content. -->
		<header class="session-bar">
			<button
				type="button"
				class="icon"
				onclick={() => coder.showSessionsList()}
				title="Back to sessions"
				aria-label="Back to sessions"
			>
				{@render listIcon()}
			</button>
			<span class="session-bar-title" title={coder.activeSession?.title ?? ''}>
				{coder.activeSession?.title ?? 'New session'}
			</span>
			<button type="button" class="icon" onclick={onNewSession} title="New session" aria-label="New session">
				{@render plusIcon()}
			</button>
		</header>
		<div class="transcript" bind:this={scrollEl}>
			{#if coder.rows.length === 0}
				<p class="hint">
					Send a prompt to start. The agent can read files, list directories, search, and run shell commands.
				</p>
			{/if}
			{#each coder.rows as row (row.id)}
				{#if row.kind === 'user'}
					<div class="row user">
						<div class="row-label">you</div>
						<div class="bubble">{row.text}</div>
					</div>
				{:else if row.kind === 'assistant'}
					<div class="row assistant">
						<div class="row-label">coder</div>
						{#if row.thinking.length > 0}
							<!-- Reasoning trace. Open while streaming so the user
								 sees thoughts land, collapsed once the message
								 finishes (the `assistant_message_end` handler
								 flips `thinkingOpen`). The component pins the
								 inner scroll to the bottom only while pinned by
								 the user, same gesture as a chat thread. -->
							<CoderThinking
								text={row.thinking}
								open={row.thinkingOpen}
								onOpenChange={(next) => (row.thinkingOpen = next)}
								streaming={row.text.length === 0}
							/>
						{/if}
						{#if row.text.trim().length > 0}
							<!-- Trim before the visibility check so a
								 model that ends with just whitespace
								 (e.g. tool-only turn that emitted a
								 trailing `\n`) doesn't render an
								 empty grey rectangle below the
								 thinking block. The actual text we
								 hand to `CoderMarkdown` is untrimmed
								 — preserving leading whitespace is
								 the renderer's job. -->
							<div class="bubble assistant-bubble">
								<CoderMarkdown text={row.text} />
							</div>
						{/if}
					</div>
				{:else if row.kind === 'tool'}
					<div class="row tool" class:err={row.isError}>
						<div class="row-label">tool · {row.name}</div>
						<details>
							<summary>{!row.hasResult ? 'running…' : row.isError ? 'error' : 'ok'}</summary>
							<div class="block-label">args</div>
							<pre class="block">{fmtArgs(row.args)}</pre>
							{#if row.hasResult}
								<div class="block-label">result</div>
								<pre class="block">{fmtArgs(row.result)}</pre>
							{/if}
						</details>
					</div>
				{:else if row.kind === 'aborted'}
					<div class="row notice">aborted</div>
				{:else if row.kind === 'error'}
					<div class="row error" role="alert">
						<div class="row-label">error</div>
						<div class="bubble">{row.text}</div>
					</div>
				{/if}
			{/each}
		</div>
		<div class="composer">
			<textarea
				bind:this={composer}
				bind:value={coder.draft}
				placeholder={coder.busy ? 'Press Esc to stop the turn…' : 'Ask the coder…'}
				rows="3"
				disabled={coder.busy}
				onkeydown={onComposerKey}
			></textarea>
		</div>
	{/if}
</div>

{#if coder.deviceCode || coder.awaitingApproval}
	<CoderConnectModal />
{/if}

{#snippet plusIcon()}
	<svg
		viewBox="0 0 16 16"
		width="14"
		height="14"
		fill="none"
		stroke="currentColor"
		stroke-width="1.5"
		stroke-linecap="round"
		stroke-linejoin="round"
		aria-hidden="true"
	>
		<path d="M8 3v10" />
		<path d="M3 8h10" />
	</svg>
{/snippet}

{#snippet listIcon()}
	<svg
		viewBox="0 0 16 16"
		width="14"
		height="14"
		fill="none"
		stroke="currentColor"
		stroke-width="1.5"
		stroke-linecap="round"
		stroke-linejoin="round"
		aria-hidden="true"
	>
		<!-- Three-line "list" glyph. Same visual language Slack /
			 Discord / Cursor use for "all conversations". -->
		<path d="M3 4h10" />
		<path d="M3 8h10" />
		<path d="M3 12h10" />
	</svg>
{/snippet}

{#snippet trashIcon()}
	<svg
		viewBox="0 0 16 16"
		width="14"
		height="14"
		fill="none"
		stroke="currentColor"
		stroke-width="1.4"
		stroke-linecap="round"
		stroke-linejoin="round"
		aria-hidden="true"
	>
		<path d="M3 4h10" />
		<path d="M5 4V3a1 1 0 0 1 1-1h4a1 1 0 0 1 1 1v1" />
		<path d="M4.5 4l.7 9a1 1 0 0 0 1 1h3.6a1 1 0 0 0 1-1l.7-9" />
	</svg>
{/snippet}

<style>
	.panel {
		display: flex;
		flex-direction: column;
		height: 100%;
		min-height: 0;
		background: var(--m-bg-1);
		color: var(--m-fg);
	}
	.header {
		flex-shrink: 0;
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 8px;
		padding: 8px 12px;
		border-bottom: 1px solid var(--m-border);
	}
	/* Mirror `ChatPanel`'s header font: uppercase, letter-spaced,
	   11 px / muted. The coder panel layers a status dot, identity,
	   and a target chip on top of that — uniform typography keeps
	   the two right-slot tenants visually consistent without
	   stripping the extra controls coder needs. */
	.title {
		display: flex;
		align-items: center;
		gap: 6px;
		min-width: 0;
		font-weight: 600;
		text-transform: uppercase;
		letter-spacing: 0.04em;
		font-size: 11px;
		color: var(--m-fg-muted);
	}
	.label {
		color: var(--m-fg);
	}
	.who {
		text-transform: none;
		letter-spacing: 0;
		font-weight: 400;
		color: var(--m-fg-muted);
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	/* Host-vs-container indicator chip. Same monitor / container
	   glyphs the terminal tabs use (`TerminalTargetIcon`) so the
	   user reads the same visual language across the IDE. The
	   colour-mix tint on the container case keeps the boundary
	   visually obvious — running `rm -rf .` on the wrong target is
	   the kind of mistake the indicator earns its keep on. */
	.target {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		color: var(--m-fg-subtle);
		border: 1px solid var(--m-border);
		border-radius: 3px;
		padding: 1px 4px;
		height: 18px;
	}
	.target.container {
		color: var(--m-success);
		border-color: color-mix(in srgb, var(--m-success) 50%, transparent);
		background: color-mix(in srgb, var(--m-success) 10%, transparent);
	}
	.actions {
		display: flex;
		align-items: center;
		gap: 6px;
	}
	.stop {
		font: inherit;
		font-size: 11px;
		color: var(--m-warning, #d4a017);
		background: transparent;
		border: 1px solid var(--m-warning, #d4a017);
		border-radius: 3px;
		padding: 0 8px;
		height: 20px;
		line-height: 18px;
		cursor: pointer;
	}
	.stop:hover {
		background: color-mix(in srgb, var(--m-warning, #d4a017) 14%, transparent);
	}
	.icon {
		background: transparent;
		border: 0;
		color: var(--m-fg-muted);
		padding: 2px 4px;
		cursor: pointer;
		display: inline-flex;
		align-items: center;
	}
	.icon:hover {
		color: var(--m-fg);
	}
	/* Sticky in-session header with "← Sessions" + title + "+
	   new". Mirrors the chat panel's `.thread-header` shape so
	   the two right-slot tenants feel consistent. */
	.session-bar {
		flex-shrink: 0;
		display: flex;
		align-items: center;
		gap: 8px;
		padding: 6px 12px;
		border-bottom: 1px solid var(--m-border);
		background: var(--m-bg-1);
	}
	/* Both "back" and "new" sit on the strip as `.icon` buttons —
	   their styles come from the shared `.icon` rule below. */
	.session-bar-title {
		flex: 1;
		min-width: 0;
		font-size: 12px;
		font-weight: 500;
		color: var(--m-fg);
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
		text-align: center;
	}
	/* Sessions list view. Sticky header pattern matches the chat
	   panel — the section title + actions row stays glued to the
	   top while the list scrolls underneath. */
	.sessions {
		flex: 1;
		min-height: 0;
		overflow-y: auto;
		padding: 0 12px 12px;
		display: flex;
		flex-direction: column;
		gap: 8px;
	}
	.sessions-header {
		position: sticky;
		top: 0;
		z-index: 1;
		background: var(--m-bg-1);
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 8px;
		margin: 0 -12px;
		padding: 6px 12px;
		border-bottom: 1px solid var(--m-border);
	}
	.section-title {
		font-size: 11px;
		font-weight: 600;
		text-transform: uppercase;
		letter-spacing: 0.04em;
		color: var(--m-fg-muted);
	}
	.header-actions {
		display: flex;
		align-items: center;
		gap: 4px;
	}
	.session-list {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: 2px;
	}
	.session-row {
		display: flex;
		align-items: stretch;
		border-radius: 4px;
		border: 1px solid transparent;
	}
	.session-row:hover {
		background: var(--m-bg-3);
		border-color: var(--m-border);
	}
	.session-row.active {
		background: color-mix(in srgb, var(--m-accent) 12%, transparent);
		border-color: color-mix(in srgb, var(--m-accent) 50%, transparent);
	}
	.session-pick {
		flex: 1;
		min-width: 0;
		text-align: left;
		font: inherit;
		color: inherit;
		background: transparent;
		border: 0;
		cursor: pointer;
		padding: 6px 8px;
		display: flex;
		flex-direction: column;
		gap: 2px;
	}
	.session-title {
		font-size: 12px;
		color: var(--m-fg);
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.session-meta {
		font-size: 11px;
		color: var(--m-fg-subtle);
	}
	.session-delete {
		opacity: 0;
		transition: opacity 0.1s;
	}
	.session-row:hover .session-delete,
	.session-row:focus-within .session-delete {
		opacity: 1;
	}
	.empty {
		flex: 1;
		display: flex;
		flex-direction: column;
		align-items: center;
		justify-content: center;
		gap: 14px;
		padding: 24px;
		text-align: center;
	}
	.empty-lede {
		font-size: 12px;
		color: var(--m-fg-muted);
		margin: 0;
		max-width: 32ch;
		line-height: 1.5;
	}
	.primary {
		font: inherit;
		background: var(--m-accent);
		color: #fff;
		border: 0;
		border-radius: 4px;
		padding: 8px 14px;
		cursor: pointer;
	}
	.primary:hover:not(:disabled) {
		filter: brightness(1.1);
	}
	.primary:disabled {
		cursor: not-allowed;
		opacity: 0.7;
	}
	.error {
		font-size: 12px;
		color: var(--m-danger);
	}
	.transcript {
		flex: 1;
		min-height: 0;
		overflow-y: auto;
		padding: 12px;
		display: flex;
		flex-direction: column;
		gap: 12px;
	}
	.hint {
		font-size: 12px;
		color: var(--m-fg-subtle);
		margin: 0;
	}
	.row {
		display: flex;
		flex-direction: column;
		gap: 4px;
	}
	.row.notice {
		font-size: 11px;
		color: var(--m-fg-subtle);
		text-align: center;
	}
	.row-label {
		font-size: 10px;
		text-transform: uppercase;
		letter-spacing: 0.06em;
		color: var(--m-fg-subtle);
	}
	.bubble {
		font-size: 13px;
		line-height: 1.5;
		white-space: pre-wrap;
		word-break: break-word;
		background: var(--m-bg-overlay);
		border-radius: 6px;
		padding: 8px 10px;
	}
	/* Assistant replies render through `CoderMarkdown`, which emits
	   real block-level HTML. `pre-wrap` on the bubble would
	   double-up by treating the model's leading `\n` characters as
	   visible blank lines on top of the markdown's already-correct
	   paragraph spacing. */
	.assistant-bubble {
		white-space: normal;
	}
	.row.user .bubble {
		background: color-mix(in srgb, var(--m-accent) 18%, transparent);
	}
	.row.error .bubble {
		background: color-mix(in srgb, var(--m-danger) 14%, transparent);
		color: var(--m-danger);
	}
	.row.tool details {
		font-size: 12px;
		background: var(--m-bg-overlay);
		border-radius: 6px;
		padding: 6px 8px;
	}
	.row.tool.err details {
		background: color-mix(in srgb, var(--m-danger) 12%, transparent);
	}
	.row.tool summary {
		cursor: pointer;
		color: var(--m-fg-muted);
	}
	.row.tool .block-label {
		margin-top: 6px;
		font-size: 10px;
		text-transform: uppercase;
		letter-spacing: 0.06em;
		color: var(--m-fg-subtle);
	}
	.row.tool .block {
		background: var(--m-bg);
		color: var(--m-fg);
		border-radius: 4px;
		padding: 6px 8px;
		max-height: 240px;
		overflow: auto;
		font-family: var(--m-font-mono, ui-monospace, monospace);
		font-size: 11px;
		line-height: 1.4;
		margin: 4px 0 0;
		white-space: pre-wrap;
		word-break: break-word;
	}
	.composer {
		flex-shrink: 0;
		border-top: 1px solid var(--m-border);
		padding: 8px;
	}
	textarea {
		width: 100%;
		box-sizing: border-box;
		resize: vertical;
		min-height: 64px;
		max-height: 240px;
		font: inherit;
		font-size: 13px;
		line-height: 1.4;
		color: var(--m-fg);
		background: var(--m-bg);
		border: 1px solid var(--m-border);
		border-radius: 4px;
		padding: 8px 10px;
	}
	textarea:focus {
		outline: none;
		border-color: var(--m-accent);
	}
	textarea:disabled {
		opacity: 0.7;
	}
</style>
