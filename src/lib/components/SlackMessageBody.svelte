<script lang="ts">
	import { openUrl } from '@tauri-apps/plugin-opener';

	import { slack } from '../slack.svelte';
	import { userLabel } from '../protocol';
	import { collectMentionedUserIds, parseSlackMrkdwn, type BlockNode, type InlineNode } from '../util/slackMrkdwn';

	type Props = { text: string };
	let { text }: Props = $props();

	// Pure function of `text`; `$derived` memoises across re-renders so
	// the tokenizer never runs twice for the same message body. The
	// renderer below walks the resulting tree.
	const blocks = $derived(parseSlackMrkdwn(text));
	const mentionedUserIds = $derived(collectMentionedUserIds(blocks));

	// Kick off `users.info` lookups for any `<@U…>` we haven't seen.
	// Has to run from an `$effect` (not the render path) because
	// `requestUser` mutates `slack.userCache` — Svelte forbids state
	// writes during template evaluation. Once each lookup resolves the
	// cache update triggers a re-render and `peekUser` returns the
	// resolved entry below.
	$effect(() => {
		for (const userId of mentionedUserIds) {
			slack.requestUser(userId);
		}
	});

	// Slack URLs come straight from the bot, so we have to be careful:
	// only http(s) and mailto open externally, anything else is dropped.
	// `URL` parsing is the second line of defence (the tokenizer
	// already filtered to known schemes when emitting `link` tokens).
	const EXTERNAL_SCHEMES = new Set(['http:', 'https:', 'mailto:']);

	function onLinkClick(event: MouseEvent, url: string) {
		event.preventDefault();
		try {
			const parsed = new URL(url);
			if (EXTERNAL_SCHEMES.has(parsed.protocol)) {
				void openUrl(parsed.toString());
			}
		} catch {
			// malformed URL — swallow
		}
	}

	/**
	 * Resolve a `<@U…>` token to its rendered `@label`. Reads through
	 * the reactive cache; the `$effect` above kicks off the fetch and
	 * the cache write re-runs this function once the user resolves.
	 *
	 * Slack pre-fills the `|label` segment when the bot composes the
	 * mention from a username string (e.g. `@alice`); when present we
	 * use it verbatim without hitting `users.info` at all. Falls back
	 * through `display_name → real_name → username → user_id`.
	 */
	function mentionLabel(userId: string, fallback: string | null): string {
		if (fallback !== null && fallback.length > 0) {
			return fallback;
		}
		const entry = slack.peekUser(userId);
		if (entry?.state === 'resolved') {
			return userLabel(entry.user);
		}
		// loading / missing / not-yet-requested → show the raw ID.
		// Re-render will swap it in once `users.info` lands.
		return userId;
	}

	function broadcastLabel(kind: 'here' | 'channel' | 'everyone', label: string | null): string {
		if (label !== null && label.length > 0) {
			return label;
		}
		return kind;
	}
</script>

{#snippet inline(nodes: InlineNode[])}
	{#each nodes as node, i (i)}
		{#if node.type === 'text'}{node.value}{:else if node.type === 'bold'}<strong>{@render inline(node.children)}</strong
			>{:else if node.type === 'italic'}<em>{@render inline(node.children)}</em>{:else if node.type === 'strike'}<s
				>{@render inline(node.children)}</s
			>{:else if node.type === 'code'}<code class="inline-code">{node.value}</code>{:else if node.type === 'link'}<a
				href={node.url}
				class="link"
				rel="noopener noreferrer"
				onclick={(e) => onLinkClick(e, node.url)}>{node.label ?? node.url}</a
			>{:else if node.type === 'userMention'}<span class="mention" title={node.userId}
				>@{mentionLabel(node.userId, node.label)}</span
			>{:else if node.type === 'channelMention'}<span class="mention channel" title={node.channelId}
				>#{node.label ?? node.channelId}</span
			>{:else if node.type === 'broadcast'}<span class="mention broadcast"
				>@{broadcastLabel(node.kind, node.label)}</span
			>{:else if node.type === 'usergroup'}<span class="mention" title={node.id}>@{node.label ?? node.id}</span
			>{:else if node.type === 'date'}<span>{node.fallback}</span>{/if}
	{/each}
{/snippet}

<div class="msg">
	{#each blocks as block, i (i)}
		{#if block.type === 'text'}
			<div class="paragraph">{@render inline(block.children)}</div>
		{:else if block.type === 'codeblock'}
			<pre class="codeblock"><code>{block.value}</code></pre>
		{:else if block.type === 'quote'}
			<blockquote class="quote">{@render inline(block.children)}</blockquote>
		{/if}
	{/each}
</div>

<style>
	.msg {
		display: flex;
		flex-direction: column;
		gap: 6px;
		font-size: 12px;
		color: var(--m-fg);
		line-height: 1.5;
		min-width: 0;
	}
	.paragraph {
		white-space: pre-wrap;
		word-wrap: break-word;
		overflow-wrap: anywhere;
	}
	.codeblock {
		margin: 0;
		padding: 8px 10px;
		background: var(--m-bg-1);
		border: 1px solid var(--m-border);
		border-radius: 4px;
		font-family: var(--m-font-mono, monospace);
		font-size: 11px;
		line-height: 1.45;
		overflow-x: auto;
		/* `overflow-x: auto` with the default `overflow-y: visible`
		   gets silently coerced to `overflow-y: auto` per CSS spec —
		   which is why one-line snippets were sprouting a vertical
		   scrollbar (and a chunky corner square where the two bars
		   met). Pin Y to `hidden` to break that coercion. */
		overflow-y: hidden;
		white-space: pre;
		color: var(--m-fg);
		/* Thin native scrollbar for both Firefox + Chromium / Tauri's
		   webview. Default WebKit scrollbar is too tall for a chat
		   bubble; this drops it to ~6 px. */
		scrollbar-width: thin;
		scrollbar-color: var(--m-border) transparent;
	}
	.codeblock::-webkit-scrollbar {
		height: 6px;
	}
	.codeblock::-webkit-scrollbar-thumb {
		background: var(--m-border);
		border-radius: 3px;
	}
	.codeblock::-webkit-scrollbar-thumb:hover {
		background: var(--m-fg-muted);
	}
	.codeblock::-webkit-scrollbar-track {
		background: transparent;
	}
	.codeblock code {
		font: inherit;
		background: transparent;
		padding: 0;
	}
	.inline-code {
		font-family: var(--m-font-mono, monospace);
		font-size: 0.92em;
		color: var(--m-code-fg);
		background: var(--m-code-bg);
		border: 1px solid var(--m-code-border);
		padding: 0.05em 0.32em;
		border-radius: 3px;
	}
	.quote {
		margin: 0;
		padding: 2px 0 2px 10px;
		border-left: 3px solid var(--m-border);
		color: var(--m-fg-muted);
		white-space: pre-wrap;
		word-wrap: break-word;
		overflow-wrap: anywhere;
	}
	.link {
		color: var(--m-accent);
		text-decoration: none;
		word-break: break-word;
	}
	.link:hover {
		text-decoration: underline;
	}
	.mention {
		color: var(--m-accent);
		background: color-mix(in srgb, var(--m-accent) 14%, transparent);
		padding: 0 4px;
		border-radius: 3px;
		font-weight: 500;
		white-space: nowrap;
	}
	.mention.broadcast {
		color: var(--m-warning, var(--m-accent));
		background: color-mix(in srgb, var(--m-warning, var(--m-accent)) 18%, transparent);
	}
	.mention.channel {
		color: var(--m-fg);
		background: var(--m-bg-3);
	}
</style>
