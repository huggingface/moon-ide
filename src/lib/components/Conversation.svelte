<script lang="ts">
	// The top-level comments of an issue or pull request, oldest
	// first, with review summaries among them. Read-only: replying
	// happens on GitHub, one click away on each entry.
	import { openUrl } from '@tauri-apps/plugin-opener';
	import { githubMarkdown, openMarkdownLinks } from '../githubMarkdown';
	import { relativeTime } from '../inboxSections';
	import type { ConversationComment } from '../protocol';
	import Pill from './Pill.svelte';
	import ExternalLinkIcon from './icons/ExternalLinkIcon.svelte';

	type Props = { comments: readonly ConversationComment[] };
	let { comments }: Props = $props();

	const REVIEW = {
		approved: ['success', 'approved'],
		changes_requested: ['danger', 'requested changes'],
		commented: ['neutral', 'reviewed'],
		dismissed: ['neutral', 'review dismissed'],
	} as const;

	// A long comment (a bot's review summary runs to pages) is cut to
	// a few lines until asked for, so the entries after it stay in
	// reach. Keyed by URL + time: stable across a re-read.
	const key = (comment: ConversationComment) => `${comment.url}\n${comment.createdAt}`;
	let expanded = $state<Record<string, boolean>>({});
	let overflowing = $state<Record<string, boolean>>({});

	function measure(el: HTMLElement, id: string) {
		const check = () => {
			overflowing[id] = el.scrollHeight > el.clientHeight + 1;
		};
		const observer = new ResizeObserver(check);
		observer.observe(el);
		check();
		return () => observer.disconnect();
	}

	const now = Date.now();
</script>

<ol class="conversation">
	{#each comments as comment (key(comment))}
		{@const id = key(comment)}
		<li class="entry" class:review={comment.review !== null}>
			<div class="meta">
				<strong>{comment.author ?? 'someone'}</strong>
				{#if comment.review !== null}
					<Pill tone={REVIEW[comment.review][0]}>{REVIEW[comment.review][1]}</Pill>
				{/if}
				<span class="time" title={comment.createdAt}>{relativeTime(comment.createdAt, now)}</span>
				<span class="spacer"></span>
				<button class="icon-btn" title="Open on GitHub" onclick={() => void openUrl(comment.url)}>
					<ExternalLinkIcon size={12} />
				</button>
			</div>
			{#if comment.body.trim() !== ''}
				<!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions (delegated link clicks; the links themselves are focusable) -->
				<div
					class="markdown-body body"
					class:clamped={!expanded[id]}
					use:githubMarkdown={comment.body}
					{@attach (el) => measure(el, id)}
					onclick={openMarkdownLinks}
				></div>
				{#if overflowing[id] || expanded[id]}
					<button class="more" onclick={() => (expanded[id] = !expanded[id])}>
						{expanded[id] ? 'Show less' : 'Show all'}
					</button>
				{/if}
			{/if}
		</li>
	{/each}
</ol>

<style>
	.conversation {
		margin: var(--m-space-2) 0 0;
		padding: 0;
		list-style: none;
		display: flex;
		flex-direction: column;
		gap: var(--m-space-2);
		max-width: 860px;
	}
	.entry {
		padding: var(--m-space-2) var(--m-space-3);
		border: 1px solid var(--m-border);
		border-radius: var(--m-radius-md);
		background: var(--m-bg);
	}
	.meta {
		display: flex;
		align-items: center;
		gap: var(--m-space-2);
		font-size: var(--m-text-sm);
	}
	.meta strong {
		font-weight: 600;
	}
	.time {
		color: var(--m-fg-subtle);
	}
	.spacer {
		flex: 1;
	}
	.body {
		margin-top: var(--m-space-2);
		min-width: 0;
		overflow-wrap: anywhere;
	}
	.body.clamped {
		max-height: 220px;
		overflow: hidden;
	}
	.more {
		margin-top: var(--m-space-1);
		padding: 0 var(--m-space-2);
		border: none;
		border-radius: var(--m-radius-sm);
		background: transparent;
		color: var(--m-accent);
		font: inherit;
		font-size: var(--m-text-sm);
		cursor: pointer;
	}
	.more:hover {
		background: var(--m-bg-2);
	}
	.icon-btn {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		width: 22px;
		height: 22px;
		padding: 0;
		border: none;
		border-radius: var(--m-radius-md);
		background: transparent;
		color: var(--m-fg-muted);
		cursor: pointer;
	}
	.icon-btn:hover {
		background: var(--m-bg-3);
		color: var(--m-fg);
	}
</style>
