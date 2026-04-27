<script lang="ts">
	import { onMount } from 'svelte';
	import { confirm } from '@tauri-apps/plugin-dialog';
	import { slack } from '../slack.svelte';
	import { botLabel, type SlackBotProfile } from '../protocol';
	import ChatConnectModal from './ChatConnectModal.svelte';

	onMount(() => {
		// Probe on mount whether the panel is visible or not — the
		// status bar's chat indicator wants the latest connection
		// state regardless of whether the user has opened the panel.
		void slack.refreshStatus();
	});

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
	}

	async function onSwitchBot() {
		await slack.clearBotSelection();
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
				<p class="placeholder">Sessions and messages will appear here in 11.1.</p>
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
	.placeholder {
		font-size: 11px;
		color: var(--m-fg-subtle);
		text-align: center;
		margin: 8px 0 0;
		font-style: italic;
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
