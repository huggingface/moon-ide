<script lang="ts">
	// A GitHub user's picture. Users have one at a stable address; an
	// app (`moon-bot-app`) has none there, so the inbox's copy is used
	// when it knows the login, and the initial when nothing loads.
	import { inbox } from '../inbox.svelte';

	type Props = { login: string | null; size?: number };
	let { login, size = 20 }: Props = $props();

	const known = $derived(
		login === null ? null : (inbox.snapshot?.items.find((item) => item.author === login)?.authorAvatarUrl ?? null),
	);
	const src = $derived.by(() => {
		if (known !== null) {
			return `${known}${known.includes('?') ? '&' : '?'}s=${size * 2}`;
		}
		return login === null ? null : `https://github.com/${encodeURIComponent(login)}.png?size=${size * 2}`;
	});
	let failed = $state<string | null>(null);
</script>

{#if src !== null && failed !== src}
	<img class="avatar" {src} alt="" width={size} height={size} onerror={() => (failed = src)} />
{:else}
	<span class="avatar initial" style:width="{size}px" style:height="{size}px" aria-hidden="true">
		{(login ?? '?').slice(0, 1).toUpperCase()}
	</span>
{/if}

<style>
	.avatar {
		flex-shrink: 0;
		border-radius: 50%;
		background: var(--m-bg-3);
	}
	.initial {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		color: var(--m-fg-muted);
		font-size: var(--m-text-xs);
		font-weight: 600;
	}
</style>
