<script lang="ts">
	import { onMount, tick } from 'svelte';
	import { confirm } from '@tauri-apps/plugin-dialog';
	import { coder, type CoderRow } from '../coder.svelte';
	import { slack } from '../slack.svelte';
	import { workspace } from '../state.svelte';
	import CoderConnectModal from './CoderConnectModal.svelte';
	import CoderMarkdown from './CoderMarkdown.svelte';
	import CoderThinking from './CoderThinking.svelte';
	import TerminalTargetIcon from './TerminalTargetIcon.svelte';
	import ContextRing from './ContextRing.svelte';
	import ChatBubbleIcon from './icons/ChatBubbleIcon.svelte';
	import SignOutIcon from './icons/SignOutIcon.svelte';
	import PlusIcon from './icons/PlusIcon.svelte';
	import ListIcon from './icons/ListIcon.svelte';
	import FileIcon from './icons/FileIcon.svelte';
	import TrashIcon from './icons/TrashIcon.svelte';
	import CodeIcon from './icons/CodeIcon.svelte';
	import { ipc } from '../ipc';
	import { formatError } from '../protocol';
	import { textInputUndo } from '../actions/textInputUndo';

	let scrollEl: HTMLDivElement | undefined = $state();
	let composer: HTMLTextAreaElement | undefined = $state();

	onMount(() => {
		void coder.refreshStatus();
	});

	// Keep the store's `composerEl` reference in sync with the
	// live textarea node — the store needs it so Ctrl+L can
	// insert an `@`-token at the caret without prop-drilling a
	// callback. Setting / clearing happens on every mount and
	// unmount of the textarea (it remounts when the user swaps
	// between session view and the sessions list).
	$effect(() => {
		coder.composerEl = composer ?? null;
		return () => {
			if (coder.composerEl === composer) {
				coder.composerEl = null;
			}
		};
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

	// Pull focus into the composer whenever the store bumps its
	// focus tick (e.g. Ctrl+L from the editor pushes a selection
	// onto `coder.attachments` and wants the user typing
	// straight away). Reads the count to register the dep; the
	// `tick()` defers until after the panel re-renders the
	// chips, otherwise the focus call could race the textarea
	// being remounted.
	let lastFocusTick = $state(0);
	$effect(() => {
		const t = coder.composerFocusTick;
		if (t === lastFocusTick) {
			return;
		}
		lastFocusTick = t;
		void tick().then(() => {
			composer?.focus();
		});
	});

	async function onComposerKey(event: KeyboardEvent) {
		// Enter sends; Shift+Enter inserts a newline. Esc aborts the
		// active turn (matches the panel header's stop button).
		// Ctrl+Z / Ctrl+Shift+Z / Ctrl+Y are wired by the
		// `use:textInputUndo` action on the textarea below.
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

	function onComposerInput(event: Event): void {
		const ta = event.currentTarget as HTMLTextAreaElement;
		coder.draft = ta.value;
	}

	// State → DOM sync for *external* draft writes only: a folder
	// switch (the bucket getter returns a different bucket's
	// draft), an attachment chip removal (`removeAttachment`
	// regex-replaces tokens out of the draft), or a post-send
	// clear. During plain typing the `oninput` handler above
	// keeps `coder.draft` and `composer.value` in lockstep, so
	// this effect's `value !== composer.value` check fails and
	// it does nothing — which is exactly what preserves the
	// textarea's native Ctrl+Z buffer. Going the other way (using
	// `bind:value`) made Svelte's binding effect periodically
	// reassign `composer.value`, and any JS write to a textarea's
	// `value` clears its native undo history; that's the bug
	// this whole approach side-steps.
	$effect(() => {
		const value = coder.draft;
		if (composer && composer.value !== value) {
			composer.value = value;
		}
	});

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

	// Live tick fed into running tool rows so their elapsed-time
	// readout (`running… (Xs)`) advances. One shared interval per
	// panel — every tool row reads the same `nowTick` and computes
	// its own elapsed locally. We pause the interval when no tool
	// is running so an idle panel doesn't burn a wakeup per
	// second, and restart it the moment a fresh `tool_call`
	// arrives. 250ms keeps the readout feeling live without
	// turning into a stopwatch ("0.0 / 0.2 / 0.5 / 0.7…");
	// formatting clamps to one decimal so 4.2s reads cleanly even
	// when the underlying tick lands at 4.247s.
	let nowTick = $state(Date.now());
	const hasRunningTool = $derived(coder.rows.some((row) => row.kind === 'tool' && !row.hasResult));
	$effect(() => {
		if (!hasRunningTool) {
			return;
		}
		nowTick = Date.now();
		const handle = window.setInterval(() => {
			nowTick = Date.now();
		}, 250);
		return () => window.clearInterval(handle);
	});

	/** Format an elapsed duration in milliseconds for the tool row
	 *  summary line. Two display regimes:
	 *
	 *  - **Live (still running)**: sub-second values render as
	 *    "Xms" so a flash-fast `read_file` reads honestly; once
	 *    we cross 1s we switch to whole-second resolution
	 *    (`floor`) so the counter ticks "1s → 2s → 3s" rather
	 *    than chasing the 250ms sample boundary ("0.8 → 1.2 →
	 *    1.5"). Beyond a minute we go to "Mm SSs".
	 *  - **Final (`hasResult`)**: same shape, except sub-minute
	 *    values keep one decimal so "1.2s" / "12ms" — captures
	 *    the precise duration the user wants for spotting slow
	 *    tools after the fact.
	 */
	function fmtElapsed(ms: number, live: boolean): string {
		if (ms < 0) {
			return '0ms';
		}
		if (ms < 1000) {
			return `${Math.round(ms)}ms`;
		}
		if (ms < 60_000) {
			if (live) {
				return `${Math.floor(ms / 1000)}s`;
			}
			return `${(ms / 1000).toFixed(1)}s`;
		}
		const min = Math.floor(ms / 60_000);
		const sec = Math.floor((ms % 60_000) / 1000);
		return `${min}m ${sec.toString().padStart(2, '0')}s`;
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

	/** Open a session's raw JSONL trace in the editor as a host-direct
	 *  file (same machinery as Ctrl+O for files outside the workspace).
	 *  Works for parent sessions and sub-agent ids alike — both live
	 *  under the active folder's slug on disk. The trace lives on the
	 *  *host* `XDG_DATA_HOME` even when the project is running in a
	 *  container, which is exactly what the host-direct file path
	 *  delivers.
	 *
	 *  Empty / never-persisted sessions surface as a flash via the
	 *  backend's `not found` error; the user can keep working. */
	async function onOpenTrace(event: MouseEvent | null, id: string): Promise<void> {
		event?.stopPropagation();
		let path: string;
		try {
			path = await ipc.coder.sessionJsonlPath(id);
		} catch (err) {
			workspace.flash(`Could not open trace: ${formatError(err)}`);
			return;
		}
		await workspace.openHostFile(path);
	}

	function baseName(path: string): string {
		const trimmed = path.replace(/\/+$/, '');
		const idx = trimmed.lastIndexOf('/');
		return idx >= 0 ? trimmed.slice(idx + 1) : trimmed;
	}

	async function onOpenAttachment(attachment: { path: string; startLine: number }): Promise<void> {
		// Open the file and jump to the first line of the captured
		// range. We don't try to restore the original column / end
		// line — the chip is "show me the context I attached", not
		// a full re-selection gesture. `jumpTo` handles the open
		// + nav-history bookkeeping (same path Ctrl/Cmd-click
		// goto-definition takes), so Alt+Left after the chip click
		// returns to wherever the user was.
		await workspace.jumpTo(attachment.path, { line: attachment.startLine - 1, character: 0 });
	}

	type ParsedAttachment = { path: string; startLine: number; endLine: number };

	/** Pull the trailing `<context>...</context>` block out of a user
	 *  message and parse its `<code_selection>` children into clickable
	 *  references. The context block always sits at the very end of
	 *  the prompt (see `renderPromptWithAttachments`), preceded by
	 *  exactly two newlines — so we anchor the regex to `$` rather
	 *  than scanning the whole text. Malformed input falls through:
	 *  on a parse miss we just render the raw text and skip chip
	 *  rendering, never crash on edge cases (model echoing back
	 *  `<context>` in an answer, partial buffers during streaming,
	 *  etc.). */
	function parseUserPrompt(text: string): { prose: string; attachments: ParsedAttachment[] } {
		const match = text.match(/(?:\n\n)?<context>\n([\s\S]*?)\n<\/context>\s*$/);
		if (!match) {
			return { prose: text, attachments: [] };
		}
		const proseEnd = match.index ?? 0;
		const prose = text.slice(0, proseEnd);
		const inner = match[1] ?? '';
		const selectionRe = /<code_selection\s+path="([^"]*)"\s+lines="([^"]*)">/g;
		const attachments: ParsedAttachment[] = [];
		let m: RegExpExecArray | null;
		while ((m = selectionRe.exec(inner)) !== null) {
			const rawPath = m[1];
			const range = m[2];
			if (rawPath === undefined || range === undefined) {
				continue;
			}
			const path = unescapeXmlAttr(rawPath);
			const dash = range.indexOf('-');
			let startLine: number;
			let endLine: number;
			if (dash >= 0) {
				startLine = parseInt(range.slice(0, dash), 10);
				endLine = parseInt(range.slice(dash + 1), 10);
			} else {
				startLine = parseInt(range, 10);
				endLine = startLine;
			}
			if (Number.isFinite(startLine) && Number.isFinite(endLine) && startLine > 0) {
				attachments.push({ path, startLine, endLine });
			}
		}
		return { prose, attachments };
	}

	function unescapeXmlAttr(s: string): string {
		return s.replaceAll('&quot;', '"').replaceAll('&lt;', '<').replaceAll('&gt;', '>').replaceAll('&amp;', '&');
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
			<!-- Rolling context-window indicator: arc fills as the
				 next round-trip's prompt grows, ticks into warning /
				 danger before auto-compaction kicks in, and pulses
				 while a compaction summary is being written. -->
			<ContextRing usage={coder.tokenUsage} compaction={coder.compaction} />
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
				<ChatBubbleIcon />
			</button>
			{#if coder.signedIn}
				<button type="button" class="icon" title="Sign out" aria-label="Sign out" onclick={onSignOut}>
					<SignOutIcon />
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
						<PlusIcon />
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
								class="icon session-row-action"
								title="Open trace in editor"
								aria-label="Open trace in editor"
								onclick={(event) => onOpenTrace(event, session.id)}
							>
								<CodeIcon />
							</button>
							<button
								type="button"
								class="icon session-row-action"
								title="Delete session"
								aria-label="Delete session"
								onclick={(event) => onDeleteSession(event, session.id, session.title)}
							>
								<TrashIcon />
							</button>
						</li>
					{/each}
				</ul>
			{/if}
		</div>
	{:else if coder.view === 'session'}
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
				<ListIcon />
			</button>
			<span class="session-bar-title" title={coder.activeSession?.title ?? ''}>
				{coder.activeSession?.title ?? 'New session'}
			</span>
			{#if coder.activeSession}
				<button
					type="button"
					class="icon"
					onclick={() => onOpenTrace(null, coder.activeSession!.id)}
					title="Open trace in editor"
					aria-label="Open trace in editor"
				>
					<CodeIcon />
				</button>
			{/if}
			<button type="button" class="icon" onclick={onNewSession} title="New session" aria-label="New session">
				<PlusIcon />
			</button>
		</header>
		<div class="transcript" bind:this={scrollEl}>
			{#if coder.rows.length === 0}
				<p class="hint">
					Send a prompt to start. The agent can read files, list directories, search, and run shell commands.
				</p>
			{/if}
			{#each coder.rows as row (row.id)}
				{@render rowMarkup(row, true)}
			{/each}
			{#if coder.compaction}
				{@render compactionMarkup(coder.compaction)}
			{/if}
		</div>
		<div class="composer">
			{#if coder.attachments.length > 0}
				<!-- Attached selections / files. Click the body to
					 open the file at the captured range; click the
					 × to remove the chip. The text snapshot the
					 chip carries gets prepended to the prompt as a
					 fenced code block on send. -->
				<div class="attachments" role="list">
					{#each coder.attachments as attachment (attachment.id)}
						<div class="attachment" role="listitem">
							<button
								type="button"
								class="attachment-open"
								title={`${attachment.path}:${attachment.startLine}-${attachment.endLine}`}
								onclick={() => onOpenAttachment(attachment)}
							>
								<FileIcon />
								<span class="attachment-label">
									<span class="attachment-name">{baseName(attachment.path)}</span>
									<span class="attachment-range">
										{attachment.startLine === attachment.endLine
											? `:${attachment.startLine}`
											: `:${attachment.startLine}-${attachment.endLine}`}
									</span>
								</span>
							</button>
							<button
								type="button"
								class="attachment-remove"
								title="Remove attachment"
								aria-label="Remove attachment"
								onclick={() => coder.removeAttachment(attachment.id)}
							>
								×
							</button>
						</div>
					{/each}
				</div>
			{/if}
			<textarea
				bind:this={composer}
				use:textInputUndo
				placeholder={coder.busy
					? 'Press Esc to stop the turn…'
					: coder.attachments.length > 0
						? 'Ask about the attached selection…'
						: 'Ask the coder…'}
				rows="3"
				disabled={coder.busy}
				onkeydown={onComposerKey}
				oninput={onComposerInput}
			></textarea>
		</div>
	{:else if coder.view === 'subagent'}
		<!-- Sub-agent pop-out: full transcript of one sub-agent, with
			 a back-arrow returning to the parent's session. No
			 composer (sub-agents take their task at spawn time and
			 finish on their own; the user can't drive them
			 mid-flight). The row renderer is the same one the
			 parent uses, so streaming sub-agents update live in
			 this view too. -->
		{@const subId = coder.viewSubagentId}
		{@const transcript = subId !== null ? (coder.subagentTranscripts.get(subId) ?? null) : null}
		<header class="session-bar">
			<button
				type="button"
				class="icon"
				onclick={() => coder.closeSubagentView()}
				title="Back to parent"
				aria-label="Back to parent"
			>
				← Back
			</button>
			{#if transcript !== null}
				<span class="session-bar-title" title={transcript.targetFolder}>
					Sub-agent · {baseName(transcript.targetFolder)}
				</span>
				<span class="subagent-mode" class:research={transcript.mode === 'research'} title="Sub-agent mode">
					{transcript.mode}
				</span>
				{#if subId !== null}
					<button
						type="button"
						class="icon"
						onclick={() => onOpenTrace(null, subId)}
						title="Open trace in editor"
						aria-label="Open trace in editor"
					>
						<CodeIcon />
					</button>
				{/if}
			{:else}
				<span class="session-bar-title">Sub-agent</span>
				<span></span>
			{/if}
		</header>
		<div class="transcript">
			{#if transcript === null}
				<p class="hint">Sub-agent transcript not available. Re-open the parent session to refresh.</p>
			{:else if transcript.rows.length === 0}
				<p class="hint">Sub-agent starting…</p>
			{:else}
				{#each transcript.rows as row (row.id)}
					{@render rowMarkup(row, false)}
				{/each}
			{/if}
		</div>
	{/if}
</div>

{#if coder.deviceCode || coder.awaitingApproval}
	<CoderConnectModal />
{/if}

<!-- Row renderer extracted as a snippet so the parent's session
	 transcript and the sub-agent pop-out can share it without
	 duplicating ~80 lines of conditional markup. `withSubagentCards`
	 controls whether `spawn_subagent` tool rows render the
	 inline collapsed card; sub-agents themselves can't spawn
	 sub-sub-agents (depth-1 cap), so the flag is `false` in the
	 sub-agent view. -->
{#snippet compactionMarkup(state: import('../coder.svelte').CompactionState)}
	<!-- Compaction disclosure: a single full-width row at the
		 bottom of the transcript so it doesn't push past the
		 user's most recent turn. While the fast-model summary
		 call is in flight the row shows a "compacting…" pip; on
		 completion it flips to a `<details>` with the synthetic
		 summary that the agent now sees in place of the older
		 middle of the history. -->
	<div class="row compaction" class:running={state.phase === 'running'}>
		<div class="row-label">compaction</div>
		{#if state.phase === 'running'}
			<div class="bubble">
				Compacting older turns into a summary
				{#if state.messagesCompacted > 0}
					({state.messagesCompacted} messages)
				{/if}
				…
			</div>
		{:else}
			<details class="compaction-details">
				<summary>
					Compacted {state.messagesCompacted} earlier message{state.messagesCompacted === 1 ? '' : 's'} into a summary
				</summary>
				<CoderMarkdown text={state.summary} />
			</details>
		{/if}
	</div>
{/snippet}

{#snippet rowMarkup(row: CoderRow, withSubagentCards: boolean)}
	{#if row.kind === 'user'}
		{@const parsed = parseUserPrompt(row.text)}
		<div class="row user">
			<div class="row-label">you</div>
			{#if parsed.prose.trim().length > 0}
				<div class="bubble">{parsed.prose}</div>
			{/if}
			{#if parsed.attachments.length > 0}
				<!-- The context block the user attached at send
								 time, rendered as clickable chips instead
								 of a verbatim XML wall in the bubble.
								 Clicking opens the file at the captured
								 starting line — the file may have changed
								 since (the agent likely just edited it),
								 so this is a "navigate to the spot I
								 referenced" gesture, not "show me what I
								 sent". -->
				<div class="user-refs">
					{#each parsed.attachments as ref, i (ref.path + ':' + ref.startLine + '-' + ref.endLine + ':' + i)}
						<button
							type="button"
							class="user-ref"
							title={`${ref.path}:${ref.startLine}-${ref.endLine}`}
							onclick={() =>
								onOpenAttachment({
									path: ref.path,
									startLine: ref.startLine,
								})}
						>
							<FileIcon />
							<span class="user-ref-label">
								<span class="user-ref-name">{baseName(ref.path)}</span>
								<span class="user-ref-range"
									>{ref.startLine === ref.endLine ? `:${ref.startLine}` : `:${ref.startLine}-${ref.endLine}`}</span
								>
							</span>
						</button>
					{/each}
				</div>
			{/if}
		</div>
	{:else if row.kind === 'assistant'}
		<!-- Skip the whole row when an assistant turn produced
		     neither thinking nor text. Tool-only turns (the model
		     emits a tool call and nothing else) used to render an
		     orphan "coder" label above the tool row, which read as
		     duplicate noise. See the `kind === 'tool'` branch
		     below for the affordance the user actually cares
		     about in that case. -->
		{@const hasThinking = row.thinking.length > 0}
		{@const hasText = row.text.trim().length > 0}
		{#if hasThinking || hasText}
			<div class="row assistant">
				<div class="row-label">coder</div>
				{#if hasThinking}
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
						streaming={!hasText}
					/>
				{/if}
				{#if hasText}
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
		{/if}
	{:else if row.kind === 'tool'}
		{@const subagent = withSubagentCards ? (coder.subagentSummaries.get(row.id) ?? null) : null}
		{@const elapsedMs = row.hasResult ? (row.durationMs ?? 0) : Math.max(0, nowTick - row.startedAt)}
		<div class="row tool" class:err={row.isError}>
			<div class="row-label">tool · {row.name}</div>
			<details>
				<summary>
					<span class="tool-status">{!row.hasResult ? 'running…' : row.isError ? 'error' : 'ok'}</span>
					<!-- Live elapsed counter while running, precise
									 final duration once the tool settles. Reads
									 the panel-level `nowTick` so every running
									 tool row advances on the same 250ms beat
									 instead of each one fighting for its own
									 interval. -->
					<span class="tool-elapsed" class:running={!row.hasResult}>{fmtElapsed(elapsedMs, !row.hasResult)}</span>
				</summary>
				<div class="block-label">args</div>
				<pre class="block">{fmtArgs(row.args)}</pre>
				{#if row.hasResult}
					<div class="block-label">result</div>
					<pre class="block">{fmtArgs(row.result)}</pre>
				{/if}
			</details>
			{#if subagent !== null}
				<!-- Collapsed sub-agent card. Renders inline under
								 the parent's `spawn_subagent` tool row so
								 the parent transcript stays scannable while
								 a click pops out into the full sub-agent
								 transcript view (`coder.view = 'subagent'`).
								 The mode badge inverts colour scheme by
								 mode so a research / agent mix-up is
								 obvious at a glance. -->
				<button
					type="button"
					class="subagent-card"
					class:done={subagent.status === 'done'}
					class:running={subagent.status === 'running'}
					class:err={subagent.status === 'error'}
					title={`Open sub-agent transcript (${subagent.targetFolder})`}
					onclick={() => coder.openSubagent(subagent.id)}
				>
					<div class="subagent-card-header">
						<span class="subagent-mode" class:research={subagent.mode === 'research'}>
							{subagent.mode}
						</span>
						<span class="subagent-folder" title={subagent.targetFolder}>
							{baseName(subagent.targetFolder)}
						</span>
						<span class="subagent-status">
							{#if subagent.status === 'running'}
								running…
							{:else if subagent.status === 'error'}
								error
							{:else}
								done
							{/if}
						</span>
					</div>
					{#if subagent.resultPreview && subagent.resultPreview.length > 0}
						<div class="subagent-preview">{subagent.resultPreview}</div>
					{/if}
					<div class="subagent-footer">
						<span class="subagent-tokens">~{subagent.tokensUsedEstimate} tok</span>
						<span class="subagent-open">Open transcript →</span>
					</div>
				</button>
			{/if}
		</div>
	{:else if row.kind === 'aborted'}
		<div class="row notice">aborted</div>
	{:else if row.kind === 'error'}
		<div class="row error" role="alert">
			<div class="row-label">error</div>
			<div class="bubble">{row.text}</div>
		</div>
	{/if}
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
	.session-row-action {
		opacity: 0;
		transition: opacity 0.1s;
	}
	.session-row:hover .session-row-action,
	.session-row:focus-within .session-row-action {
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
	/* Inline references attached to a user message. Sit just below
	   the prose bubble and read as quiet "links" rather than
	   primary content — the referenced code may have been edited
	   by the agent already, so these are nav affordances first,
	   citations second. */
	.user-refs {
		display: flex;
		flex-wrap: wrap;
		gap: 4px;
	}
	.user-ref {
		font: inherit;
		font-size: 11px;
		display: inline-flex;
		align-items: center;
		gap: 4px;
		padding: 2px 6px;
		background: var(--m-bg-overlay);
		color: var(--m-fg-muted);
		border: 1px solid var(--m-border);
		border-radius: 10px;
		cursor: pointer;
		max-width: 100%;
	}
	.user-ref:hover {
		color: var(--m-fg);
		background: color-mix(in srgb, var(--m-accent) 14%, transparent);
		border-color: color-mix(in srgb, var(--m-accent) 40%, var(--m-border));
	}
	.user-ref-label {
		display: inline-flex;
		align-items: baseline;
		gap: 1px;
		min-width: 0;
	}
	.user-ref-name {
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.user-ref-range {
		color: var(--m-fg-subtle);
		flex-shrink: 0;
	}
	.row.error .bubble {
		background: color-mix(in srgb, var(--m-danger) 14%, transparent);
		color: var(--m-danger);
	}
	.row.compaction .bubble {
		background: var(--m-bg-overlay);
		color: var(--m-fg-muted);
		font-style: italic;
	}
	.row.compaction.running .bubble {
		animation: compaction-pulse 1.6s ease-in-out infinite;
	}
	@keyframes compaction-pulse {
		0%,
		100% {
			opacity: 1;
		}
		50% {
			opacity: 0.55;
		}
	}
	.compaction-details {
		font-size: 12px;
		background: var(--m-bg-overlay);
		border-radius: 6px;
		padding: 6px 8px;
		color: var(--m-fg-muted);
	}
	.compaction-details summary {
		cursor: pointer;
		color: var(--m-fg-muted);
	}
	.compaction-details[open] summary {
		margin-bottom: 6px;
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
		display: flex;
		align-items: baseline;
		gap: 6px;
	}
	.row.tool .tool-status {
		flex: 0 0 auto;
	}
	/* Tabular numerics on the elapsed counter so the trailing
	   digits don't shift the layout while a running tool ticks
	   from 1.2s → 1.3s. `running` flips to a tinted muted colour
	   so a finished call's duration reads as a settled fact, not
	   an active timer. */
	.row.tool .tool-elapsed {
		flex: 0 0 auto;
		color: var(--m-fg-subtle);
		font-size: 11px;
		font-variant-numeric: tabular-nums;
	}
	.row.tool .tool-elapsed.running {
		color: var(--m-accent);
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
	/* Collapsed sub-agent card. Renders inline under the parent's
	   `spawn_subagent` tool row. Reads as a clickable "pop out"
	   affordance — click anywhere on the card body, and the panel
	   swaps to the sub-agent's own transcript view. */
	.subagent-card {
		appearance: none;
		display: flex;
		flex-direction: column;
		gap: 4px;
		margin-top: 6px;
		padding: 8px 10px;
		background: var(--m-bg-overlay);
		border: 1px solid var(--m-border);
		border-radius: 6px;
		color: var(--m-fg);
		text-align: left;
		font: inherit;
		font-size: 12px;
		cursor: pointer;
	}
	.subagent-card:hover {
		background: var(--m-bg-2);
		border-color: var(--m-border-strong, var(--m-border));
	}
	.subagent-card.running {
		border-style: dashed;
	}
	.subagent-card.err {
		border-color: var(--m-danger);
		background: color-mix(in srgb, var(--m-danger) 10%, transparent);
	}
	.subagent-card-header {
		display: flex;
		align-items: center;
		gap: 8px;
		font-size: 11px;
		color: var(--m-fg-muted);
	}
	.subagent-folder {
		flex: 1;
		min-width: 0;
		font-weight: 500;
		font-family: var(--m-font-mono, ui-monospace, monospace);
		color: var(--m-fg);
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.subagent-status {
		font-size: 10px;
		text-transform: uppercase;
		letter-spacing: 0.06em;
	}
	/* Mode pill: `agent` keeps the accent fill (full toolkit, can
	   edit), `research` flips to a quieter neutral fill so the
	   read-only constraint is visually distinct. Same shape as
	   the SCM panel's changes badge for visual continuity. */
	.subagent-mode {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		min-width: 0;
		padding: 1px 6px;
		border-radius: 999px;
		background: var(--m-accent);
		color: var(--m-bg);
		font-size: 10px;
		font-weight: 600;
		text-transform: uppercase;
		letter-spacing: 0.06em;
		flex-shrink: 0;
	}
	.subagent-mode.research {
		background: var(--m-bg-3, var(--m-bg-2));
		color: var(--m-fg-muted);
	}
	.subagent-preview {
		font-size: 12px;
		line-height: 1.4;
		color: var(--m-fg);
		display: -webkit-box;
		-webkit-line-clamp: 2;
		line-clamp: 2;
		-webkit-box-orient: vertical;
		overflow: hidden;
	}
	.subagent-footer {
		display: flex;
		align-items: center;
		justify-content: space-between;
		font-size: 10px;
		color: var(--m-fg-subtle);
	}
	.subagent-open {
		font-weight: 500;
		color: var(--m-fg-muted);
	}
	.composer {
		flex-shrink: 0;
		border-top: 1px solid var(--m-border);
		padding: 8px;
		display: flex;
		flex-direction: column;
		gap: 6px;
	}
	.attachments {
		display: flex;
		flex-wrap: wrap;
		gap: 4px;
	}
	.attachment {
		display: inline-flex;
		align-items: stretch;
		font-size: 11px;
		background: var(--m-bg-overlay);
		border: 1px solid var(--m-border);
		border-radius: 4px;
		max-width: 100%;
		min-width: 0;
	}
	.attachment-open {
		font: inherit;
		font-size: 11px;
		display: inline-flex;
		align-items: center;
		gap: 4px;
		padding: 2px 4px 2px 6px;
		background: transparent;
		border: 0;
		color: var(--m-fg);
		cursor: pointer;
		min-width: 0;
		max-width: 220px;
	}
	.attachment-open:hover {
		background: color-mix(in srgb, var(--m-accent) 14%, transparent);
	}
	.attachment-label {
		display: inline-flex;
		align-items: baseline;
		gap: 1px;
		min-width: 0;
	}
	.attachment-name {
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.attachment-range {
		color: var(--m-fg-muted);
		flex-shrink: 0;
	}
	.attachment-remove {
		font: inherit;
		font-size: 13px;
		line-height: 1;
		color: var(--m-fg-muted);
		background: transparent;
		border: 0;
		border-left: 1px solid var(--m-border);
		padding: 0 6px;
		cursor: pointer;
	}
	.attachment-remove:hover {
		color: var(--m-fg);
		background: color-mix(in srgb, var(--m-danger) 14%, transparent);
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
