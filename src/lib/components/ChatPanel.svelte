<script lang="ts">
	import { onMount, untrack } from 'svelte';
	import { confirm } from '@tauri-apps/plugin-dialog';
	import { openUrl } from '@tauri-apps/plugin-opener';
	import { slack } from '../slack.svelte';
	import { botLabel, userLabel, type SlackBotProfile, type SlackSession } from '../protocol';
	import { formatSlackRelative, formatSlackTime } from '../util/slackTime';
	import { collectMentionedUserIds, parseSlackMrkdwn, slackPlainText } from '../util/slackMrkdwn';
	import ChatConnectModal from './ChatConnectModal.svelte';
	import SlackMessageBody from './SlackMessageBody.svelte';

	onMount(() => {
		// Probe on mount whether the panel is visible or not — the
		// status bar's chat indicator wants the latest connection
		// state regardless of whether the user has opened the panel.
		void slack.refreshStatus();
	});

	// Auto-fetch sessions and the active thread whenever we have a
	// connected bot and the panel is actually visible. Wrapped in
	// `untrack` for the bot-id read so a single bot-card render
	// doesn't pull `sessions` into this effect's dep set.
	$effect(() => {
		const visible = slack.panelVisible;
		const bot = slack.activeBot;
		const connected = slack.connected;
		if (!visible || !connected || bot === null) {
			return;
		}
		untrack(() => {
			if (slack.sessions === null && !slack.loadingSessions) {
				void slack.loadSessions();
			}
			const ts = slack.activeThreadTs;
			if (ts !== null && slack.threadMessages === null && !slack.loadingThread) {
				void slack.loadThread(ts);
			}
		});
	});

	// Re-render relative timestamps every minute so "2 min" doesn't
	// stay frozen at "just now". Cheap; only the strings change.
	let nowTick = $state(new Date());
	$effect(() => {
		const id = setInterval(() => {
			nowTick = new Date();
		}, 30_000);
		return () => clearInterval(id);
	});

	const activeSession = $derived(
		slack.activeThreadTs === null ? null : (slack.sessions?.find((s) => s.thread_ts === slack.activeThreadTs) ?? null),
	);

	// Collect every `<@U…>` referenced in the session list previews so
	// we can pre-warm the user cache. Done from an `$effect` so the
	// render path stays pure (mutating `userCache` during render trips
	// `state_unsafe_mutation`). The resulting cache writes re-trigger
	// `previewOf` below and the labels swap in once `users.info`
	// resolves.
	const sessionMentionUserIds = $derived.by(() => {
		const sessions = slack.sessions;
		if (sessions === null) {
			return [];
		}
		const ids = new Set<string>();
		for (const session of sessions) {
			if (session.user_id !== null) {
				ids.add(session.user_id);
			}
			for (const id of collectMentionedUserIds(parseSlackMrkdwn(session.preview))) {
				ids.add(id);
			}
		}
		return Array.from(ids);
	});

	$effect(() => {
		for (const id of sessionMentionUserIds) {
			slack.requestUser(id);
		}
	});

	function resolveCachedUser(userId: string): string | null {
		const entry = slack.peekUser(userId);
		if (entry?.state === 'resolved') {
			return userLabel(entry.user);
		}
		return null;
	}

	function previewOf(session: SlackSession): string {
		const flat = slackPlainText(session.preview, { resolveUserId: resolveCachedUser }).trim();
		if (flat.length > 0) {
			return flat;
		}
		return session.reply_count > 0 ? '(no preview, see thread)' : '(empty message)';
	}

	function senderLabel(userId: string | null, isBot: boolean): string {
		if (isBot) {
			return slack.activeBot ? botLabel(slack.activeBot) : 'Bot';
		}
		if (userId !== null && userId === slack.status?.identity?.user_id) {
			return 'You';
		}
		if (userId !== null) {
			return resolveCachedUser(userId) ?? userId;
		}
		return 'Unknown';
	}

	async function onDisconnect() {
		const ok = await confirm('Disconnect Slack? Your token will be removed from the OS keyring.', {
			title: 'Disconnect Slack',
			okLabel: 'Disconnect',
			cancelLabel: 'Cancel',
		});
		if (!ok) {
			return;
		}
		await slack.disconnect();
	}

	async function onPickBot(profile: SlackBotProfile) {
		await slack.selectBot(profile);
		void slack.loadSessions();
	}

	async function onSwitchBot() {
		await slack.clearBotSelection();
	}

	function onSelectSession(threadTs: string) {
		slack.selectThread(threadTs);
	}

	function onBackToSessions() {
		slack.selectThread(null);
	}

	function onRefreshSessions() {
		void slack.loadSessions();
	}

	function onRefreshThread() {
		const ts = slack.activeThreadTs;
		if (ts !== null) {
			void slack.loadThread(ts);
		}
	}

	// Same scheme allowlist as the message-body link handler — bot
	// footers always point at `https://` URLs (HF Hub, trace viewer,
	// download), but the renderer is paranoid by design and also
	// rejects anything Slack returns with a `value`-only payload
	// (those buttons are filtered server-side, but cheap to belt-and-
	// suspenders here too).
	const ACTION_SCHEMES = new Set(['http:', 'https:', 'mailto:']);

	function onActionClick(event: MouseEvent, url: string) {
		event.preventDefault();
		try {
			const parsed = new URL(url);
			if (ACTION_SCHEMES.has(parsed.protocol)) {
				void openUrl(parsed.toString());
			}
		} catch {
			// Malformed URL — silently swallow rather than crashing
			// the click handler.
		}
	}
</script>

<aside class="chat-panel" data-region="chat" aria-label="Chat panel">
	<header>
		<div class="title">Chat</div>
		{#if slack.connected && slack.status?.identity}
			<button type="button" class="link" onclick={onDisconnect}>Disconnect</button>
		{/if}
	</header>

	{#if !slack.status}
		<div class="empty">Loading…</div>
	{:else if !slack.connected}
		<div class="empty">
			<p class="empty-lede">Connect Slack to chat with a bot from the IDE.</p>
			<p class="empty-detail">
				moon-ide DMs Slack on your behalf — the bot already runs in your workspace, you just need a personal token to
				talk to it.
			</p>
			<button type="button" class="primary" onclick={() => slack.openConnectModal()}>Connect Slack</button>
		</div>
	{:else}
		<div class="connected">
			<section class="card">
				<div class="card-row">
					<span class="card-label">Connected as</span>
					<span class="card-value">{slack.status.identity?.user_name ?? '—'}</span>
				</div>
				<div class="card-row muted">
					<span class="card-label">Workspace</span>
					<span class="card-value">{slack.status.identity?.team ?? '—'}</span>
				</div>
			</section>

			{#if slack.activeBot}
				<section class="card bot-card">
					<header class="bot-header">
						<div class="bot-id">
							{#if slack.activeBot.image_url}
								<img src={slack.activeBot.image_url} alt="" class="avatar" />
							{:else}
								<div class="avatar avatar-placeholder" aria-hidden="true">{botLabel(slack.activeBot)[0] ?? '?'}</div>
							{/if}
							<div class="bot-text">
								<div class="bot-name">{botLabel(slack.activeBot)}</div>
								<div class="bot-handle">@{slack.activeBot.username}</div>
							</div>
						</div>
						<button type="button" class="link" onclick={onSwitchBot}>Switch bot</button>
					</header>
				</section>

				{#if slack.activeThreadTs === null}
					<section class="card sessions-card">
						<header class="section-header">
							<span class="section-title">Sessions</span>
							<button
								type="button"
								class="link"
								onclick={onRefreshSessions}
								disabled={slack.loadingSessions}
								title="Reload sessions">Refresh</button
							>
						</header>
						{#if slack.loadingSessions && slack.sessions === null}
							<div class="muted-row">
								<div class="spinner" aria-hidden="true"></div>
								<span>Loading sessions…</span>
							</div>
						{:else if slack.sessionsError}
							<p class="card-error">{slack.sessionsError}</p>
						{:else if slack.sessions && slack.sessions.length === 0}
							<p class="card-detail">
								No sessions yet. Start one by DMing
								<strong>{botLabel(slack.activeBot)}</strong>
								from regular Slack — sending will land in the IDE in 11.3.
							</p>
						{:else if slack.sessions}
							<ul class="session-list">
								{#each slack.sessions as session (session.thread_ts)}
									<li>
										<button type="button" class="session-row" onclick={() => onSelectSession(session.thread_ts)}>
											<div class="session-preview">{previewOf(session)}</div>
											<div class="session-meta">
												{#key nowTick}
													<span class="session-time">{formatSlackRelative(session.latest_ts)}</span>
												{/key}
												{#if session.reply_count > 0}
													<span class="session-replies"
														>{session.reply_count} {session.reply_count === 1 ? 'reply' : 'replies'}</span
													>
												{/if}
											</div>
										</button>
									</li>
								{/each}
							</ul>
						{/if}
					</section>
				{:else}
					<section class="card thread-card">
						<header class="section-header">
							<button type="button" class="back-button" onclick={onBackToSessions} title="Back to sessions">
								← Sessions
							</button>
							<button
								type="button"
								class="link"
								onclick={onRefreshThread}
								disabled={slack.loadingThread}
								title="Reload thread">Refresh</button
							>
						</header>
						{#if activeSession}
							<p class="thread-subject">{previewOf(activeSession)}</p>
						{/if}
						{#if slack.loadingThread && slack.threadMessages === null}
							<div class="muted-row">
								<div class="spinner" aria-hidden="true"></div>
								<span>Loading thread…</span>
							</div>
						{:else if slack.threadError}
							<p class="card-error">{slack.threadError}</p>
						{:else if slack.threadMessages && slack.threadMessages.length === 0}
							<p class="card-detail">No messages in this thread yet.</p>
						{:else if slack.threadMessages}
							<ol class="message-list">
								{#each slack.threadMessages as message (message.ts)}
									<li class="message" class:from-bot={message.is_bot}>
										<header class="message-header">
											<span class="message-author">{senderLabel(message.user_id, message.is_bot)}</span>
											<span class="message-time">
												{formatSlackTime(message.ts)}
												{#if message.edited_ts}
													<span class="message-edited" title="Edited">· edited</span>
												{/if}
											</span>
										</header>
										<div class="message-body"><SlackMessageBody text={message.text} /></div>
										{#if message.actions.length > 0}
											<div class="message-actions">
												{#each message.actions as action, i (i)}
													<button
														type="button"
														class="action-btn"
														class:primary={action.style === 'primary'}
														class:danger={action.style === 'danger'}
														onclick={(e) => onActionClick(e, action.url)}
														title={action.url}>{action.label}</button
													>
												{/each}
											</div>
										{/if}
									</li>
								{/each}
							</ol>
						{/if}
					</section>
				{/if}
			{:else if slack.loadingBots}
				<section class="card center">
					<div class="spinner" aria-hidden="true"></div>
					<p class="card-lede">Scanning your 50 most recent DMs for bots…</p>
					<p class="card-detail">
						One-time setup — your pick is saved across launches. Slack doesn't expose a user-search API for
						<code>xoxp-</code>
						tokens, so we walk your DM list instead.
					</p>
				</section>
			{:else if slack.botError}
				<section class="card">
					<p class="card-lede">Couldn't load bots from your 50 most recent DMs.</p>
					<p class="card-error">{slack.botError}</p>
					<button type="button" class="link" onclick={() => slack.discoverBots()}>Retry</button>
				</section>
			{:else if slack.botCandidates.length === 0}
				<section class="card">
					<p class="card-lede">No bots in your 50 most recent DMs.</p>
					<p class="card-detail">
						DM your bot from regular Slack (or send a quick "hi" if your DM with it is older than your 50 most recent),
						then click Refresh.
					</p>
					<button type="button" class="link" onclick={() => slack.discoverBots()}>Refresh</button>
				</section>
			{:else}
				<section class="card picker">
					<p class="card-lede">Pick a bot to chat with</p>
					<p class="card-detail">
						Bots from your 50 most recent Slack DMs. Click one to make it the active bot for this IDE.
					</p>
					<ul class="bot-list">
						{#each slack.botCandidates as bot (bot.user_id)}
							<li>
								<button type="button" class="bot-row" onclick={() => onPickBot(bot)}>
									{#if bot.image_url}
										<img src={bot.image_url} alt="" class="avatar" />
									{:else}
										<div class="avatar avatar-placeholder" aria-hidden="true">{botLabel(bot)[0] ?? '?'}</div>
									{/if}
									<div class="bot-text">
										<div class="bot-name">{botLabel(bot)}</div>
										<div class="bot-handle">@{bot.username}</div>
									</div>
								</button>
							</li>
						{/each}
					</ul>
					<button type="button" class="link refresh" onclick={() => slack.discoverBots()}>Rescan DMs</button>
				</section>
			{/if}
		</div>
	{/if}
</aside>

<ChatConnectModal />

<style>
	.chat-panel {
		display: flex;
		flex-direction: column;
		min-width: 0;
		min-height: 0;
		height: 100%;
		background: var(--m-bg-1);
		color: var(--m-fg);
		font-size: 12px;
	}
	header {
		display: flex;
		align-items: center;
		justify-content: space-between;
		padding: 8px 12px;
		border-bottom: 1px solid var(--m-border);
		flex-shrink: 0;
	}
	.title {
		font-weight: 600;
		text-transform: uppercase;
		letter-spacing: 0.04em;
		font-size: 11px;
		color: var(--m-fg-muted);
	}
	.empty {
		flex: 1;
		display: flex;
		flex-direction: column;
		align-items: center;
		justify-content: center;
		gap: 12px;
		padding: 20px;
		text-align: center;
	}
	.empty-lede {
		margin: 0;
		font-size: 13px;
		color: var(--m-fg);
	}
	.empty-detail {
		margin: 0;
		font-size: 12px;
		color: var(--m-fg-muted);
		line-height: 1.5;
		max-width: 32ch;
	}
	.primary {
		font: inherit;
		background: var(--m-accent);
		border: 1px solid var(--m-accent);
		color: #fff;
		padding: 6px 14px;
		border-radius: 4px;
		cursor: pointer;
	}
	.connected {
		flex: 1;
		overflow-y: auto;
		padding: 12px;
		display: flex;
		flex-direction: column;
		gap: 12px;
	}
	.card {
		background: var(--m-bg-2);
		border: 1px solid var(--m-border);
		border-radius: 6px;
		padding: 10px 12px;
		display: flex;
		flex-direction: column;
		gap: 6px;
	}
	.card.center {
		align-items: center;
		text-align: center;
		gap: 10px;
		padding: 16px 12px;
	}
	.card-row {
		display: flex;
		justify-content: space-between;
		align-items: baseline;
		gap: 12px;
	}
	.card-row.muted {
		color: var(--m-fg-muted);
	}
	.card-label {
		font-size: 11px;
		text-transform: uppercase;
		letter-spacing: 0.04em;
		color: var(--m-fg-muted);
	}
	.card-value {
		font-size: 12px;
		color: var(--m-fg);
		text-align: right;
		min-width: 0;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.card-lede {
		margin: 0;
		font-size: 12px;
		color: var(--m-fg);
	}
	.card-detail {
		margin: 0;
		font-size: 11px;
		color: var(--m-fg-muted);
		line-height: 1.5;
	}
	.card-error {
		font-size: 12px;
		color: var(--m-danger);
		margin: 0;
	}
	.bot-card {
		gap: 10px;
	}
	.bot-header {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 8px;
		padding: 0;
		border-bottom: 0;
	}
	.bot-id {
		display: flex;
		align-items: center;
		gap: 10px;
		min-width: 0;
	}
	.avatar {
		width: 32px;
		height: 32px;
		border-radius: 6px;
		object-fit: cover;
		flex-shrink: 0;
	}
	.avatar-placeholder {
		display: flex;
		align-items: center;
		justify-content: center;
		background: var(--m-bg-3);
		color: var(--m-fg-muted);
		font-weight: 600;
		text-transform: uppercase;
	}
	.bot-text {
		min-width: 0;
		display: flex;
		flex-direction: column;
		gap: 2px;
	}
	.bot-name {
		font-size: 13px;
		font-weight: 600;
		color: var(--m-fg);
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.bot-handle {
		font-size: 11px;
		color: var(--m-fg-muted);
		font-family: var(--m-font-mono, monospace);
	}
	.bot-list {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: 4px;
	}
	.bot-row {
		display: flex;
		align-items: center;
		gap: 10px;
		width: 100%;
		padding: 6px 8px;
		background: transparent;
		border: 1px solid transparent;
		border-radius: 4px;
		cursor: pointer;
		text-align: left;
		font: inherit;
		color: inherit;
	}
	.bot-row:hover {
		background: var(--m-bg-3);
		border-color: var(--m-border);
	}
	.refresh {
		align-self: flex-start;
	}
	.link {
		font: inherit;
		background: transparent;
		border: 0;
		color: var(--m-accent);
		text-decoration: underline;
		cursor: pointer;
		padding: 0;
		font-size: 12px;
	}
	.section-header {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 8px;
		padding: 0;
		border-bottom: 0;
	}
	.section-title {
		font-size: 11px;
		font-weight: 600;
		text-transform: uppercase;
		letter-spacing: 0.04em;
		color: var(--m-fg-muted);
	}
	.muted-row {
		display: flex;
		align-items: center;
		gap: 8px;
		color: var(--m-fg-muted);
		font-size: 12px;
	}
	.session-list,
	.message-list {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: 2px;
	}
	.session-row {
		display: flex;
		flex-direction: column;
		align-items: flex-start;
		gap: 4px;
		width: 100%;
		padding: 6px 8px;
		background: transparent;
		border: 1px solid transparent;
		border-radius: 4px;
		cursor: pointer;
		text-align: left;
		font: inherit;
		color: inherit;
	}
	.session-row:hover {
		background: var(--m-bg-3);
		border-color: var(--m-border);
	}
	.session-preview {
		font-size: 12px;
		color: var(--m-fg);
		overflow: hidden;
		text-overflow: ellipsis;
		display: -webkit-box;
		-webkit-line-clamp: 2;
		line-clamp: 2;
		-webkit-box-orient: vertical;
		line-height: 1.4;
		max-width: 100%;
	}
	.session-meta {
		display: flex;
		align-items: center;
		gap: 8px;
		font-size: 11px;
		color: var(--m-fg-muted);
	}
	.session-replies::before {
		content: '·';
		margin-right: 8px;
		color: var(--m-fg-subtle);
	}
	.thread-card {
		gap: 8px;
	}
	.back-button {
		font: inherit;
		background: transparent;
		border: 0;
		color: var(--m-accent);
		cursor: pointer;
		padding: 0;
		font-size: 12px;
	}
	.thread-subject {
		margin: 0;
		font-size: 12px;
		color: var(--m-fg-muted);
		font-style: italic;
		overflow: hidden;
		text-overflow: ellipsis;
		display: -webkit-box;
		-webkit-line-clamp: 2;
		line-clamp: 2;
		-webkit-box-orient: vertical;
		padding-bottom: 4px;
		border-bottom: 1px solid var(--m-border);
	}
	.message {
		display: flex;
		flex-direction: column;
		gap: 2px;
		padding: 6px 8px;
		border-radius: 4px;
	}
	.message.from-bot {
		background: var(--m-bg-3);
	}
	.message-header {
		display: flex;
		align-items: baseline;
		justify-content: space-between;
		gap: 8px;
	}
	.message-author {
		font-size: 11px;
		font-weight: 600;
		color: var(--m-fg);
	}
	.message-time {
		font-size: 10px;
		color: var(--m-fg-muted);
		font-variant-numeric: tabular-nums;
	}
	.message-edited {
		margin-left: 4px;
		color: var(--m-fg-subtle);
	}
	.message-body {
		font-size: 12px;
		color: var(--m-fg);
		line-height: 1.5;
		white-space: pre-wrap;
		word-wrap: break-word;
	}
	.message-actions {
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
		margin-top: 4px;
	}
	.action-btn {
		font-size: 11px;
		font-weight: 500;
		padding: 4px 10px;
		border: 1px solid var(--m-border);
		border-radius: 4px;
		background: var(--m-bg-1);
		color: var(--m-fg);
		cursor: pointer;
		line-height: 1.2;
	}
	.action-btn:hover {
		background: var(--m-bg-2);
		border-color: var(--m-border-strong);
	}
	.action-btn.primary {
		color: var(--m-accent);
		border-color: color-mix(in srgb, var(--m-accent) 40%, var(--m-border));
		background: color-mix(in srgb, var(--m-accent) 10%, transparent);
	}
	.action-btn.primary:hover {
		background: color-mix(in srgb, var(--m-accent) 18%, transparent);
	}
	.action-btn.danger {
		color: var(--m-danger);
		border-color: color-mix(in srgb, var(--m-danger) 40%, var(--m-border));
		background: color-mix(in srgb, var(--m-danger) 10%, transparent);
	}
	.action-btn.danger:hover {
		background: color-mix(in srgb, var(--m-danger) 18%, transparent);
	}
	.spinner {
		width: 18px;
		height: 18px;
		border: 2px solid var(--m-border);
		border-top-color: var(--m-accent);
		border-radius: 50%;
		animation: spin 0.8s linear infinite;
	}
	@keyframes spin {
		to {
			transform: rotate(360deg);
		}
	}
</style>
