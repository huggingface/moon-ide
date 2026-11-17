<script lang="ts">
	// The way into the GitHub inbox (ADR 0112), at the top of the
	// sidebar: the inbox is where a working session starts, so its
	// counts are always in view.
	import { inbox } from '../inbox.svelte';
	import { workspace } from '../state.svelte';
	import { INBOX_PATH } from '../util/inboxPath';
	import InboxIcon from './icons/InboxIcon.svelte';

	const active = $derived(workspace.activePath === INBOX_PATH);
	const counts = $derived(inbox.counts);
</script>

<button
	type="button"
	class="inbox-button"
	class:active
	title="GitHub inbox (Ctrl+Shift+G)"
	onclick={() => workspace.openInboxTab()}
>
	<InboxIcon size={15} />
	<span class="label">Inbox</span>
	<span class="badges">
		{#if counts.unread > 0}
			<span class="badge unread" title="{counts.unread} unread">{counts.unread}</span>
		{/if}
		{#if counts.review > 0}
			<span class="badge review" title="{counts.review} waiting for your review">{counts.review}</span>
		{/if}
		{#if counts.watched > 0}
			<span class="badge watched" title="{counts.watched} open from watched authors">{counts.watched}</span>
		{/if}
	</span>
</button>

<style>
	.inbox-button {
		display: flex;
		align-items: center;
		gap: var(--m-space-2);
		margin: var(--m-space-2);
		padding: 0 var(--m-space-3);
		height: 34px;
		flex-shrink: 0;
		border: 1px solid var(--m-border);
		border-radius: var(--m-radius-md);
		background: var(--m-bg-2);
		color: var(--m-fg);
		font: inherit;
		font-size: var(--m-text-md);
		font-weight: 600;
		text-align: left;
		cursor: pointer;
	}
	.inbox-button:hover {
		border-color: var(--m-border-strong);
		background: var(--m-bg-3);
	}
	.inbox-button.active {
		border-color: var(--m-accent);
	}
	.label {
		flex: 1;
	}
	.badges {
		display: inline-flex;
		gap: var(--m-space-1);
	}
	.badge {
		--badge-color: var(--m-fg-muted);
		min-width: 20px;
		padding: 0 6px;
		height: 18px;
		border-radius: var(--m-radius-pill);
		background: color-mix(in srgb, var(--badge-color) 18%, transparent);
		color: var(--badge-color);
		font-size: var(--m-text-xs);
		font-weight: 600;
		line-height: 18px;
		text-align: center;
		font-variant-numeric: tabular-nums;
	}
	.unread {
		--badge-color: var(--m-accent);
	}
	.review {
		--badge-color: var(--m-warning);
	}
	.watched {
		--badge-color: var(--m-merged);
	}
</style>
