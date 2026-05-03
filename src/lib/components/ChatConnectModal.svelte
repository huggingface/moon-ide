<script lang="ts">
	import { tick } from 'svelte';
	import { openUrl } from '@tauri-apps/plugin-opener';
	import { slack } from '../slack.svelte';

	let token = $state('');
	let connectError = $state<string | null>(null);
	let pasteInput: HTMLInputElement | undefined = $state();

	$effect(() => {
		if (slack.showConnectModal) {
			void tick().then(() => pasteInput?.focus());
		} else {
			token = '';
			connectError = null;
		}
	});

	async function onSubmit(event: SubmitEvent) {
		event.preventDefault();
		const trimmed = token.trim();
		if (!trimmed) {
			connectError = 'Paste your User OAuth token first (starts with "xoxp-").';
			return;
		}
		connectError = null;
		const result = await slack.connect(trimmed);
		if (result.ok) {
			slack.closeConnectModal();
			return;
		}
		connectError = result.error;
	}

	function onCancel() {
		slack.closeConnectModal();
	}

	async function openLink(href: string) {
		try {
			await openUrl(href);
		} catch {
			// Falling silent on a help-link failure is fine — the user
			// can still type the URL into a browser themselves.
		}
	}

	function onBackdropKey(event: KeyboardEvent) {
		if (event.key === 'Escape') {
			slack.closeConnectModal();
		}
	}
</script>

{#if slack.showConnectModal}
	<div
		class="backdrop"
		role="dialog"
		aria-modal="true"
		aria-labelledby="slack-connect-title"
		tabindex="-1"
		onkeydown={onBackdropKey}
	>
		<div class="modal">
			<header>
				<h2 id="slack-connect-title">Connect Slack</h2>
				<button type="button" class="close" aria-label="Close" onclick={onCancel}>×</button>
			</header>
			<p class="lede">
				moon-ide's chat panel DMs a Slack bot on your behalf. Paste a personal
				<strong>User OAuth Token</strong>
				(<code>xoxp-…</code>) and we'll keep it in your OS keyring — never on disk in plaintext.
			</p>
			<p class="prereq">
				<strong>Before you start:</strong> DM the bot you want to chat with at least once from regular Slack, and make
				sure that DM is in your <strong>50 most recent</strong>. Slack doesn't expose a user-search API for
				<code>xoxp-</code>
				tokens, so we walk your DM list instead — capped at 50 to keep the first-connect scan fast. If your bot's DM is older,
				just send it a quick "hi" from regular Slack to bump it.
			</p>
			<ol class="steps">
				<li>
					Create a Slack app at
					<button type="button" class="link" onclick={() => openLink('https://api.slack.com/apps')}>
						api.slack.com/apps
					</button>
					→ "From scratch".
				</li>
				<li>
					In <strong>OAuth &amp; Permissions → User Token Scopes</strong>, add all of:
					<ul class="scopes">
						<li><code>chat:write</code> — post messages</li>
						<li><code>im:history</code> — read DM messages</li>
						<li><code>im:read</code> — list DM channels</li>
						<li><code>im:write</code> — mark messages read</li>
						<li><code>users:read</code> — read DM partners' profiles (find which are bots)</li>
						<li><code>team:read</code> — show your workspace icon on the chat panel</li>
						<li><code>reactions:read</code> — see bot status reactions (✅ ⚠️ ❌)</li>
						<li><code>reactions:write</code> — react to messages</li>
						<li><code>files:read</code> — see attachments the bot sends</li>
						<li><code>files:write</code> — upload images / files to the bot</li>
					</ul>
					<span class="scopes-note">
						All ten are claimed upfront so you don't have to revisit OAuth &amp; Permissions every time a new capability
						ships. If you ever see a <code>missing_scope</code>
						error in the panel, add the scope here, click <strong>Reinstall to Workspace</strong>, then disconnect &amp;
						reconnect from the IDE.
					</span>
				</li>
				<li>
					Click <strong>Install to Workspace</strong> and authorize.
				</li>
				<li>
					Copy the
					<strong>User OAuth Token</strong>
					(starts with <code>xoxp-</code>; <em>not</em> the Bot token) and paste it below.
				</li>
			</ol>
			<form onsubmit={onSubmit}>
				<label for="slack-token">User OAuth Token</label>
				<input
					id="slack-token"
					bind:this={pasteInput}
					bind:value={token}
					type="password"
					placeholder="xoxp-…"
					autocomplete="off"
					autocorrect="off"
					autocapitalize="off"
					spellcheck="false"
					disabled={slack.connecting}
				/>
				{#if connectError}
					<p class="error" role="alert">{connectError}</p>
				{/if}
				<div class="actions">
					<button type="button" class="ghost" onclick={onCancel} disabled={slack.connecting}>Cancel</button>
					<button type="submit" class="primary" disabled={slack.connecting}>
						{slack.connecting ? 'Connecting…' : 'Connect'}
					</button>
				</div>
			</form>
		</div>
	</div>
{/if}

<style>
	.backdrop {
		position: fixed;
		inset: 0;
		background: rgba(0, 0, 0, 0.4);
		display: flex;
		align-items: center;
		justify-content: center;
		z-index: 100;
	}
	.modal {
		background: var(--m-bg-1);
		border: 1px solid var(--m-border-strong);
		border-radius: 8px;
		box-shadow: 0 12px 48px rgba(0, 0, 0, 0.5);
		width: min(560px, 92vw);
		max-height: 90vh;
		overflow-y: auto;
		padding: 18px 20px 20px;
		color: var(--m-fg);
	}
	header {
		display: flex;
		align-items: center;
		justify-content: space-between;
		margin-bottom: 8px;
	}
	header h2 {
		font-size: 14px;
		font-weight: 600;
		margin: 0;
	}
	.close {
		font: inherit;
		font-size: 18px;
		line-height: 1;
		background: transparent;
		border: 0;
		color: var(--m-fg-muted);
		cursor: pointer;
		padding: 0 6px;
	}
	.close:hover {
		color: var(--m-fg);
	}
	.lede {
		font-size: 12px;
		color: var(--m-fg-muted);
		margin: 0 0 12px;
		line-height: 1.5;
	}
	.prereq {
		font-size: 12px;
		color: var(--m-fg);
		background: var(--m-bg-2);
		border: 1px solid var(--m-border);
		border-radius: 4px;
		padding: 8px 10px;
		margin: 0 0 12px;
		line-height: 1.5;
	}
	.steps {
		font-size: 12px;
		color: var(--m-fg);
		margin: 0 0 14px;
		padding-left: 20px;
		line-height: 1.6;
	}
	.steps code {
		font-family: var(--m-font-mono, monospace);
		font-size: 11px;
		background: var(--m-bg-2);
		padding: 1px 4px;
		border-radius: 3px;
	}
	.scopes {
		margin: 4px 0 4px;
		padding-left: 18px;
		list-style: disc;
		color: var(--m-fg-muted);
	}
	.scopes li {
		margin: 2px 0;
		line-height: 1.5;
	}
	.scopes-note {
		display: block;
		font-size: 11px;
		color: var(--m-fg-subtle);
		margin-top: 4px;
		font-style: italic;
		line-height: 1.5;
	}
	.link {
		font: inherit;
		background: transparent;
		border: 0;
		padding: 0;
		color: var(--m-accent);
		text-decoration: underline;
		cursor: pointer;
	}
	form {
		display: flex;
		flex-direction: column;
		gap: 6px;
	}
	form label {
		font-size: 11px;
		font-weight: 500;
		color: var(--m-fg-muted);
		text-transform: uppercase;
		letter-spacing: 0.04em;
	}
	form input {
		font: inherit;
		font-family: var(--m-font-mono, monospace);
		background: var(--m-bg);
		border: 1px solid var(--m-border-strong);
		border-radius: 4px;
		padding: 8px 10px;
		color: var(--m-fg);
	}
	form input:focus {
		outline: 2px solid var(--m-accent);
		outline-offset: -1px;
	}
	.error {
		font-size: 12px;
		color: var(--m-danger);
		margin: 0;
		white-space: pre-wrap;
	}
	.actions {
		display: flex;
		justify-content: flex-end;
		gap: 8px;
		margin-top: 8px;
	}
	.actions button {
		font: inherit;
		padding: 6px 14px;
		border-radius: 4px;
		cursor: pointer;
	}
	.actions .ghost {
		background: transparent;
		border: 1px solid var(--m-border-strong);
		color: var(--m-fg-muted);
	}
	.actions .ghost:hover:not(:disabled) {
		color: var(--m-fg);
	}
	.actions .primary {
		background: var(--m-accent);
		border: 1px solid var(--m-accent);
		color: #fff;
	}
	.actions .primary:disabled,
	.actions .ghost:disabled {
		opacity: 0.5;
		cursor: not-allowed;
	}
</style>
