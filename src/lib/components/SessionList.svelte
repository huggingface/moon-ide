<script lang="ts">
	// The active project's coder sessions, always on screen in the
	// sidebar: picking a row opens it in the coder panel beside it.
	import { confirm } from '@tauri-apps/plugin-dialog';
	import { coder } from '../coder.svelte';
	import { ipc } from '../ipc';
	import { frontendLog } from '../logs.svelte';
	import { formatError, type CoderSessionSummary } from '../protocol';
	import {
		goToSessionBranch,
		openSessionTrace,
		openSessionTraceOnHub,
		revealComposer,
		sessionBranch,
		startNewSession,
		worktreeFolderForBranch,
	} from '../sessionActions';
	import {
		groupSessions,
		isDismissable,
		sessionGroupOf,
		sessionRowState,
		type SessionGroupId,
		type SessionRowState,
	} from '../sessionGroups';
	import { sessionDismissals } from '../sessionDismissals.svelte';
	import { workspace } from '../state.svelte';
	import SessionRenameInput from './SessionRenameInput.svelte';
	import BranchIcon from './icons/BranchIcon.svelte';
	import CheckIcon from './icons/CheckIcon.svelte';
	import CloudUploadIcon from './icons/CloudUploadIcon.svelte';
	import CodeIcon from './icons/CodeIcon.svelte';
	import EditIcon from './icons/EditIcon.svelte';
	import ExternalLinkIcon from './icons/ExternalLinkIcon.svelte';
	import PlusIcon from './icons/PlusIcon.svelte';
	import TrashIcon from './icons/TrashIcon.svelte';

	// Creating a worktree is slow (git worktree add + lock, and a
	// container repair when the workspace is containerised), so the
	// button shows a spinner + disables until it lands — otherwise a
	// click looks like it did nothing for a second or two.
	let creatingWorktree = $state(false);

	const worktreeButtonTitle = $derived(
		creatingWorktree
			? 'Creating isolated worktree…'
			: `New isolated session — a new branch off ${workspace.gitBranch.defaultBranchRemoteRef ?? 'the default branch'}, in its own git worktree under the project`,
	);

	async function onNewWorktreeSession(): Promise<void> {
		// Spin up an isolated session in its own git worktree
		// (ADR 0028) so it can't collide with other agents in this
		// folder. The folder bar grows a nested row for the worktree.
		if (creatingWorktree) {
			return;
		}
		creatingWorktree = true;
		try {
			await workspace.newCoderWorktreeSession();
		} finally {
			creatingWorktree = false;
		}
		revealComposer();
	}

	async function onPickSession(id: string): Promise<void> {
		coder.setPanelVisible(true);
		// Opening replays the transcript from disk; for the session
		// already on screen that would only throw away its scroll
		// position.
		if (coder.current.visibleSessionId !== id || coder.view !== 'session') {
			await coder.openSession(id);
		}
		revealComposer();
	}

	// Transcript search. The query goes to the backend
	// (`coder_search_sessions` scans the on-disk JSONL transcripts —
	// summaries carry no transcript text) which returns matching
	// session ids; we filter the already-loaded list by id. `null`
	// matches = no active filter (empty query). Debounced so a fast
	// typist doesn't fan out one disk scan per keystroke.
	let sessionSearch = $state('');
	let sessionSearchMatches = $state<Set<string> | null>(null);

	$effect(() => {
		const query = sessionSearch.trim();
		// Depend on the active folder too: the backend searches *its*
		// active folder, so a folder switch must re-run the search
		// rather than filter the new folder's list with stale ids.
		void coder.activeFolderPath;
		if (query === '') {
			sessionSearchMatches = null;
			return;
		}
		const timer = setTimeout(async () => {
			try {
				const ids = await ipc.coder.searchSessions(query);
				// Drop stale responses: only apply if the query the
				// round-trip was issued for is still what's typed.
				if (sessionSearch.trim() === query) {
					sessionSearchMatches = new Set(ids);
				}
			} catch (err) {
				frontendLog('moon-ide', 'warn', `coder: session search failed: ${formatError(err)}`);
			}
		}, 200);
		return () => clearTimeout(timer);
	});

	const filteredSessions = $derived.by(() => {
		const sessions = coder.sessions;
		const matches = sessionSearchMatches;
		if (sessions === null) {
			return null;
		}
		if (matches === null) {
			return sessions;
		}
		return sessions.filter((s) => matches.has(s.id));
	});

	type SessionListRow = {
		session: CoderSessionSummary;
		state: SessionRowState;
		/** Set on the first row of each section. */
		header: { group: SessionGroupId; title: string; count: number } | null;
	};

	// The list's rows in display order. Search matches stay one flat
	// list: the user is looking for a transcript, not triaging.
	const sessionListRows = $derived.by((): SessionListRow[] => {
		const rows = (filteredSessions ?? []).map((session) => ({
			session,
			state: sessionRowState({
				awaitingInput: coder.current.isSessionAwaitingInput(session.id),
				running: coder.current.isSessionRunning(session.id),
				attention: coder.current.isSessionAttention(session.id),
				lastError: Boolean(session.last_error),
				interrupted: Boolean(session.interrupted),
				dismissed: sessionDismissals.isDismissed(session),
			}),
		}));
		if (sessionSearchMatches !== null) {
			return rows.map((row) => ({ ...row, header: null }));
		}
		return groupSessions(rows, (row) => row.state).flatMap((group) =>
			group.items.map((row, index) => ({
				...row,
				header: index === 0 ? { group: group.id, title: group.title, count: group.items.length } : null,
			})),
		);
	});

	const STATE_LABEL: Record<SessionRowState, string | null> = {
		awaiting: 'needs input',
		running: 'running…',
		failed: 'last turn failed',
		interrupted: 'interrupted',
		finished: 'finished',
		idle: null,
	};

	const STATE_HINT: Record<SessionRowState, string> = {
		awaiting: 'Agent needs your input — click to answer',
		running: 'Session is running — click to follow',
		failed: 'Last turn failed — open to retry',
		interrupted: 'Turn never finished (restart/stop) — open to relaunch',
		finished: 'Finished while you were away — click to open',
		idle: 'Open session',
	};

	type HubPhase = 'idle' | 'syncing' | 'synced' | 'failed';

	function hubRowState(sessionId: string): HubPhase {
		const s = coder.hubSyncState[sessionId];
		if (!s) {
			return coder.hubBucket?.uploaded[sessionId] ? 'synced' : 'idle';
		}
		return s.phase;
	}

	function hubRowTitle(sessionId: string): string {
		const bucket = coder.hubBucket;
		if (!bucket) {
			return 'Upload to Hugging Face';
		}
		const target = `${bucket.namespace}/${bucket.name}`;
		const live = coder.hubSyncState[sessionId];
		if (live?.phase === 'syncing') {
			return `Uploading to ${target}…`;
		}
		if (live?.phase === 'failed') {
			return `Upload failed: ${live.error}`;
		}
		const marker = bucket.uploaded[sessionId];
		if (marker || live?.phase === 'synced') {
			return `Synced to ${target}`;
		}
		return `Upload to ${target}`;
	}

	async function onUploadSession(sessionId: string): Promise<void> {
		try {
			await coder.uploadSessionToHub(sessionId);
		} catch (err) {
			workspace.flash(`Hub upload failed: ${formatError(err)}`);
		}
	}

	let renamingId = $state<string | null>(null);

	/** Set sessions aside from "Needs you" without deleting them
	 *  (ADR 0114). They come back when they have new activity. */
	function dismissSessions(sessions: readonly CoderSessionSummary[]): void {
		for (const session of sessions) {
			coder.current.clearSessionAttention(session.id);
		}
		sessionDismissals.dismiss(sessions);
	}

	const dismissableSessions = $derived(
		sessionListRows.filter((row) => isDismissable(row.state)).map((row) => row.session),
	);

	async function onDeleteSession(id: string, title: string): Promise<void> {
		const ok = await confirm(`Delete session "${title || '(untitled)'}"? This cannot be undone.`, {
			title: 'Delete session',
			kind: 'warning',
		});
		if (!ok) {
			return;
		}
		await coder.deleteSession(id);
		sessionDismissals.forget(id);
	}

	const RELATIVE_FORMATTER = new Intl.RelativeTimeFormat(undefined, { numeric: 'auto' });

	// The list never leaves the screen, so its relative times have
	// to age on their own rather than on the next visit: every tick
	// re-runs `formatRelative` in the rows.
	let clockTick = $state(0);
	$effect(() => {
		const timer = setInterval(() => {
			clockTick += 1;
		}, 30_000);
		return () => clearInterval(timer);
	});

	function formatRelative(ms: number): string {
		void clockTick;
		const diff = Date.now() - ms;
		// Coarse buckets — sessions usually span minutes to days,
		// not seconds. "2m" / "3h" without pulling in date-fns.
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

<section class="sessions" aria-label="Coder sessions">
	<header class="sessions-header">
		<span class="section-title">Sessions</span>
		{#if coder.signedIn}
			<div class="header-actions">
				<button
					type="button"
					class="icon"
					class:creating={creatingWorktree}
					onclick={onNewWorktreeSession}
					disabled={creatingWorktree}
					title={worktreeButtonTitle}
					aria-label="New isolated session"
				>
					<BranchIcon />
				</button>
				<button
					type="button"
					class="icon"
					data-session-new
					onclick={startNewSession}
					title="New session"
					aria-label="New session"
				>
					<PlusIcon />
				</button>
			</div>
		{/if}
	</header>
	{#if coder.status === null}
		<p class="hint">Loading sessions…</p>
	{:else if !coder.signedIn}
		<p class="hint">Sign in to the coder to see this project's sessions.</p>
	{:else if coder.sessions === null}
		<p class="hint">Loading sessions…</p>
	{:else if coder.sessions.length === 0}
		<p class="hint">
			No sessions yet. Click <strong>+</strong> above (or send a prompt) to start a fresh conversation.
		</p>
	{:else}
		<input
			type="search"
			class="session-search"
			data-session-search
			placeholder="Search transcripts…"
			aria-label="Search session transcripts"
			bind:value={sessionSearch}
			spellcheck="false"
			autocomplete="off"
		/>
		{#if sessionListRows.length === 0}
			<p class="hint">No sessions match “{sessionSearch.trim()}”.</p>
		{:else}
			<ul class="session-list">
				{#each sessionListRows as { session, state, header } (session.id)}
					{@const isCurrent = coder.current.visibleSessionId === session.id}
					{@const renaming = renamingId === session.id}
					{@const branch = sessionBranch(session)}
					{@const hubPhase = coder.hubBucket ? hubRowState(session.id) : 'idle'}
					{#if header}
						<li class="session-group-header">
							<span class="session-group-dot {header.group}" aria-hidden="true"></span>
							{header.title}
							<span class="session-group-count">{header.count}</span>
							{#if header.group === 'needs-you' && dismissableSessions.length > 0}
								<button
									type="button"
									class="group-dismiss"
									title="Move the failed, interrupted and finished sessions to Earlier until they have new activity"
									onclick={() => dismissSessions(dismissableSessions)}
								>
									Dismiss all
								</button>
							{/if}
						</li>
					{/if}
					<li
						class="session-row {state}"
						class:active={isCurrent}
						class:needs-you={sessionGroupOf(state) === 'needs-you'}
						aria-current={isCurrent ? 'true' : undefined}
					>
						{#if renaming}
							<div class="session-title">
								<SessionRenameInput id={session.id} title={session.title} onDone={() => (renamingId = null)} />
							</div>
						{:else}
							<button
								type="button"
								class="session-pick session-title"
								onclick={() => onPickSession(session.id)}
								title={STATE_HINT[state]}
							>
								{#if state !== 'idle'}
									<span class="state-dot {state}" aria-hidden="true"
										>{state === 'failed' || state === 'interrupted' ? '!' : ''}</span
									>
								{/if}
								<span class="session-title-text" title={session.title || '(untitled)'}
									>{session.title || '(untitled)'}</span
								>
							</button>
						{/if}
						<div class="session-foot">
							<div class="session-meta">
								{#if STATE_LABEL[state] !== null}
									<span class="state-label {state}">{STATE_LABEL[state]}</span>
									<span class="session-meta-sep">·</span>
								{/if}
								{formatRelative(session.updated_at_ms)}
								{#if hubPhase !== 'idle'}
									<span class="hub-state {hubPhase}" title={hubRowTitle(session.id)}>
										<CloudUploadIcon size={11} />
									</span>
								{/if}
							</div>
							{#if branch}
								{@const inWorktree = worktreeFolderForBranch(branch) !== null}
								<button
									type="button"
									class="session-branch-chip"
									class:worktree={inWorktree}
									title={inWorktree
										? `Go to the worktree on ${branch}`
										: `Switch the project's main checkout to ${branch}`}
									aria-label={inWorktree ? `Go to worktree on ${branch}` : `Switch to branch ${branch}`}
									onclick={() => void goToSessionBranch(session)}
								>
									<BranchIcon size={11} />
									<span class="chip-name">{branch}</span>
								</button>
							{/if}
						</div>
						{#if !renaming}
							<div class="session-row-actions">
								{#if isDismissable(state)}
									<button
										type="button"
										class="icon"
										title="Dismiss: move to Earlier until there is new activity"
										aria-label="Dismiss session"
										onclick={() => dismissSessions([session])}
									>
										<CheckIcon size={12} />
									</button>
								{/if}
								<button
									type="button"
									class="icon"
									title="Open trace in editor"
									aria-label="Open trace in editor"
									onclick={() => openSessionTrace(session.id)}
								>
									<CodeIcon />
								</button>
								{#if coder.hubBucket}
									<button
										type="button"
										class="icon hub-action {hubPhase}"
										title={hubRowTitle(session.id)}
										aria-label={hubRowTitle(session.id)}
										onclick={() => onUploadSession(session.id)}
										disabled={hubPhase === 'syncing'}
									>
										<CloudUploadIcon />
									</button>
									{#if coder.hubBucket.uploaded[session.id]}
										<button
											type="button"
											class="icon"
											title="Open trace on Hugging Face (Alt-click to copy URL)"
											aria-label="Open trace on Hugging Face"
											onclick={(event) => openSessionTraceOnHub(session.id, event.altKey)}
										>
											<ExternalLinkIcon />
										</button>
									{/if}
								{/if}
								<button
									type="button"
									class="icon"
									title="Rename session"
									aria-label="Rename session"
									onclick={() => (renamingId = session.id)}
								>
									<EditIcon size={12} />
								</button>
								<button
									type="button"
									class="icon"
									title="Delete session"
									aria-label="Delete session"
									onclick={() => onDeleteSession(session.id, session.title)}
								>
									<TrashIcon />
								</button>
							</div>
						{/if}
					</li>
				{/each}
			</ul>
		{/if}
	{/if}
</section>

<style>
	.sessions {
		flex: 1;
		min-height: 0;
		display: flex;
		flex-direction: column;
		padding-top: var(--m-space-1);
	}
	.sessions-header {
		flex-shrink: 0;
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: var(--m-space-2);
		padding: 0 var(--m-space-2) 0 var(--m-space-4);
		min-height: var(--m-row-h);
	}
	.section-title {
		font-size: var(--m-text-sm);
		font-weight: 600;
		color: var(--m-fg-muted);
	}
	.header-actions {
		display: flex;
		align-items: center;
		gap: 0;
	}
	.icon {
		background: transparent;
		border: 0;
		color: var(--m-fg-muted);
		padding: 0;
		cursor: pointer;
		display: inline-flex;
		align-items: center;
		width: var(--m-icon-btn);
		height: var(--m-icon-btn);
		justify-content: center;
		border-radius: var(--m-radius-md);
	}
	.icon:hover {
		color: var(--m-fg);
		background: var(--m-bg-3);
	}
	/* Worktree-create in flight: spin the branch glyph + tint it
	   accent so the click clearly registered while git works. */
	.icon.creating {
		color: var(--m-accent);
		cursor: default;
	}
	.icon.creating :global(svg) {
		animation: worktree-spin 0.8s linear infinite;
	}
	@keyframes worktree-spin {
		to {
			transform: rotate(360deg);
		}
	}
	.hint {
		font-size: var(--m-text-sm);
		color: var(--m-fg-subtle);
		margin: 0;
		padding: var(--m-space-1) var(--m-space-4);
	}
	.session-search {
		flex-shrink: 0;
		margin: 0 var(--m-space-2) var(--m-space-1);
		min-width: 0;
		background: var(--m-bg-2);
		border: 1px solid var(--m-border);
		border-radius: var(--m-radius-md);
		color: var(--m-fg);
		font-size: var(--m-text-sm);
		padding: 0 var(--m-space-2);
		height: var(--m-control-h);
		font-family: inherit;
	}
	.session-search:focus {
		outline: none;
		border-color: var(--m-accent);
	}
	.session-search::placeholder {
		color: var(--m-fg-subtle);
	}
	.session-list {
		flex: 1;
		min-height: 0;
		overflow-y: auto;
		list-style: none;
		margin: 0;
		padding: 0 var(--m-space-2) var(--m-space-2);
		display: flex;
		flex-direction: column;
		gap: 1px;
	}
	.session-group-header {
		flex-shrink: 0;
		display: flex;
		align-items: center;
		gap: var(--m-space-2);
		margin-top: var(--m-space-3);
		padding: 0 var(--m-space-2);
		font-size: var(--m-text-sm);
		font-weight: 500;
		color: var(--m-fg-muted);
		min-height: 24px;
	}
	.session-group-header:first-child {
		margin-top: var(--m-space-1);
	}
	.session-group-dot {
		width: 6px;
		height: 6px;
		border-radius: 50%;
		background: var(--m-fg-subtle);
	}
	.session-group-dot.needs-you {
		background: var(--m-warning);
	}
	.session-group-dot.running {
		background: var(--m-accent);
	}
	.session-group-count {
		padding: 0;
		color: var(--m-fg-subtle);
		font-weight: 400;
		font-size: var(--m-text-xs);
		font-variant-numeric: tabular-nums;
	}
	/* Title on the first line; status · time and the branch chip
	   share the second, so a long branch name squeezes the meta
	   text and never the title. */
	.session-row {
		position: relative;
		flex-shrink: 0;
		display: flex;
		flex-direction: column;
		gap: 1px;
		padding: 6px var(--m-space-2);
		border-radius: var(--m-radius-md);
	}
	.session-row:hover {
		background: var(--m-bg-2);
	}
	.session-row.active {
		background: var(--m-bg-3);
	}
	.session-row:has(.session-pick:focus-visible) {
		outline: 1px solid var(--m-accent);
		outline-offset: -1px;
	}
	.session-title {
		min-width: 0;
		display: flex;
		align-items: center;
		gap: var(--m-space-2);
		font-size: var(--m-text-md);
		color: var(--m-fg);
		line-height: 18px;
	}
	.session-row.needs-you .session-title,
	.session-row.running .session-title,
	.session-row.active .session-title {
		font-weight: 500;
	}
	.session-pick {
		padding: 0;
		border: 0;
		border-radius: 0;
		background: transparent;
		text-align: left;
		cursor: pointer;
	}
	.session-pick:focus-visible {
		outline: none;
	}
	/* The whole row opens the session, not just the title line; the
	   chip and the actions sit above this. */
	.session-pick::after {
		content: '';
		position: absolute;
		inset: 0;
	}
	.session-title-text {
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
		min-width: 0;
	}
	.session-foot {
		display: flex;
		align-items: center;
		gap: var(--m-space-2);
		min-width: 0;
		min-height: 18px;
	}
	.session-meta {
		flex: 0 1 auto;
		min-width: 0;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
		font-size: var(--m-text-xs);
		color: var(--m-fg-subtle);
	}
	.session-meta-sep {
		color: var(--m-fg-subtle);
	}
	.state-label {
		font-weight: 500;
	}
	.state-label.running,
	.state-label.awaiting {
		color: var(--m-accent);
	}
	.state-label.finished,
	.state-label.interrupted {
		color: var(--m-warning);
	}
	.state-label.failed {
		color: var(--m-danger);
	}
	.state-dot {
		flex-shrink: 0;
		width: 8px;
		height: 8px;
		border-radius: 50%;
		margin: 0 1px;
	}
	/* Running pip — small accent dot that pulses while the bucket
	   reports `busy`. The session list is the only surface where a
	   user actively scans for "is anything still working?", and a
	   pulsing dot beats a static badge for that question. */
	.state-dot.running {
		background: var(--m-accent);
		box-shadow: 0 0 0 0 color-mix(in srgb, var(--m-accent) 60%, transparent);
		animation: session-running-pulse 1.4s ease-in-out infinite;
	}
	@keyframes session-running-pulse {
		0% {
			box-shadow: 0 0 0 0 color-mix(in srgb, var(--m-accent) 60%, transparent);
		}
		70% {
			box-shadow: 0 0 0 6px color-mix(in srgb, var(--m-accent) 0%, transparent);
		}
		100% {
			box-shadow: 0 0 0 0 color-mix(in srgb, var(--m-accent) 0%, transparent);
		}
	}
	/* Needs-input dot — a turn parked on an `ask_user` prompt,
	   waiting for the human. Accent hue like the running dot (it's
	   the same agent, still mid-turn) but a slower halo pulse so
	   "waiting on you" reads calmer than the working dot's brisk
	   pulse. Distinct from the static amber finished dot, which
	   means "done, go look"; this means "blocked, go answer". */
	.state-dot.awaiting {
		background: var(--m-accent);
		box-shadow: 0 0 0 0 color-mix(in srgb, var(--m-accent) 60%, transparent);
		animation: session-awaiting-pulse 2.2s ease-in-out infinite;
	}
	@keyframes session-awaiting-pulse {
		0% {
			box-shadow: 0 0 0 0 color-mix(in srgb, var(--m-accent) 55%, transparent);
		}
		70% {
			box-shadow: 0 0 0 5px color-mix(in srgb, var(--m-accent) 0%, transparent);
		}
		100% {
			box-shadow: 0 0 0 0 color-mix(in srgb, var(--m-accent) 0%, transparent);
		}
	}
	/* Finished marker — a static amber dot on a session whose turn
	   ended while the user was looking at a different session (or
	   another folder). Mirrors the folder-bar's `.done` sparkle:
	   same `--m-warning` hue, and deliberately *no* pulse — the
	   work is done, so a static cue says "this one's waiting on
	   you" without over-claiming attention. Clears when the row is
	   opened. */
	.state-dot.finished {
		background: var(--m-warning);
	}
	/* Failed-last-turn tell: red "!" so a coordinator fleet's dead
	   workers stand out from finished ones at a glance. Interrupted
	   (restart/stop) is its amber sibling. */
	.state-dot.failed,
	.state-dot.interrupted {
		width: auto;
		height: auto;
		border-radius: 0;
		font-weight: 700;
		font-size: 12px;
		line-height: 1;
	}
	.state-dot.failed {
		color: var(--m-danger);
	}
	.state-dot.interrupted {
		color: var(--m-warning);
	}
	/* Upload state of the row's trace, visible without hovering for
	   the action that changes it. */
	.hub-state {
		display: inline-flex;
		vertical-align: middle;
		margin-left: var(--m-space-1);
		color: var(--m-fg-subtle);
	}
	.hub-state.syncing {
		color: var(--m-accent);
		animation: hub-action-pulse 1.2s ease-in-out infinite;
	}
	.hub-state.failed {
		color: var(--m-danger);
	}
	/* "Switch back to this session's branch" chip (ADR 0028). Always
	   visible (unlike the hover-only icon actions) — the branch tie is
	   the row's headline metadata, not a secondary action. */
	.session-branch-chip {
		position: relative;
		z-index: 1;
		flex: 0 1 auto;
		min-width: 0;
		margin-left: auto;
		display: inline-flex;
		align-items: center;
		gap: 4px;
		padding: 0 var(--m-space-1);
		border: none;
		border-radius: var(--m-radius-sm);
		background: transparent;
		color: var(--m-fg-muted);
		font-size: var(--m-text-xs);
		cursor: pointer;
		height: 18px;
		margin-right: calc(-1 * var(--m-space-1));
	}
	.session-branch-chip :global(svg) {
		flex-shrink: 0;
	}
	.session-branch-chip:hover {
		color: var(--m-fg);
		background: var(--m-bg-overlay);
	}
	/* Worktree (isolated) sessions read with a faint accent so the
	   list distinguishes "branch lives in its own checkout" from a
	   regular session's committed-branch tie. */
	.session-branch-chip.worktree {
		color: var(--m-accent);
	}
	.session-branch-chip .chip-name {
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
		font-family: var(--m-font-mono, monospace);
	}
	.group-dismiss {
		margin-left: auto;
		margin-right: calc(-1 * var(--m-space-1));
		height: 20px;
		padding: 0 var(--m-space-2);
		border: none;
		border-radius: var(--m-radius-sm);
		background: transparent;
		color: var(--m-fg-subtle);
		font: inherit;
		font-size: var(--m-text-xs);
		font-weight: 400;
		cursor: pointer;
	}
	.group-dismiss:hover {
		background: var(--m-bg-3);
		color: var(--m-fg);
	}
	/* Laid over the end of the title line, so showing them on hover
	   or keyboard focus moves nothing. */
	.session-row-actions {
		position: absolute;
		z-index: 2;
		top: 3px;
		right: 3px;
		display: flex;
		align-items: center;
		padding: 1px;
		border-radius: var(--m-radius-md);
		background: var(--m-bg-3);
		box-shadow: 0 0 0 1px var(--m-border);
		opacity: 0;
		pointer-events: none;
		transition: opacity 0.1s;
	}
	.session-row-actions .icon {
		width: 22px;
		height: 22px;
		border-radius: var(--m-radius-sm);
	}
	.session-row-actions .icon:hover {
		background: var(--m-bg-overlay);
	}
	.session-row:hover .session-row-actions,
	.session-row:focus-within .session-row-actions {
		opacity: 1;
		pointer-events: auto;
	}
	.hub-action.syncing {
		color: var(--m-accent);
		animation: hub-action-pulse 1.2s ease-in-out infinite;
	}
	.hub-action.failed {
		color: var(--m-danger);
	}
	@keyframes hub-action-pulse {
		0%,
		100% {
			filter: brightness(1);
		}
		50% {
			filter: brightness(1.4);
		}
	}
	@media (prefers-reduced-motion: reduce) {
		.state-dot.running,
		.state-dot.awaiting,
		.hub-state.syncing,
		.hub-action.syncing {
			animation: none;
		}
	}
</style>
