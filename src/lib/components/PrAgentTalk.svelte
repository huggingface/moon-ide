<script lang="ts">
	// Talking to a pull request's agent from its tab (ADR 0124): what
	// was asked here and what it answered, as comments under the
	// pull request's own, and a box to ask the next thing. The
	// session is the same one the diff's "Explain" asks.
	import { githubMarkdown, openMarkdownLinks } from '../githubMarkdown';
	import { aboutLabel, type Talk, type TalkAbout } from '../reviewNotes';
	import SparklesIcon from './icons/SparklesIcon.svelte';
	import SpinnerIcon from './icons/SpinnerIcon.svelte';

	type Props = {
		talk: Talk;
		repoUrl: string | null;
		/** The comment or thread the next question is about. */
		about: TalkAbout | null;
		onClearAbout: () => void;
		onAsk: (question: string) => void;
		/** Why nothing can be asked (a fork's branch), else `null`. */
		unavailable: string | null;
		starting: boolean;
	};
	let { talk, repoUrl, about, onClearAbout, onAsk, unavailable, starting }: Props = $props();

	let draft = $state('');
	let box = $state<HTMLTextAreaElement | null>(null);

	// Picking a comment to ask about puts the caret here.
	$effect(() => {
		if (about !== null) {
			box?.focus();
			box?.scrollIntoView({ block: 'nearest' });
		}
	});

	function send() {
		const question = draft.trim();
		if (question === '' || unavailable !== null) {
			return;
		}
		onAsk(question);
		draft = '';
	}

	function onKeydown(event: KeyboardEvent) {
		if (event.key === 'Enter' && !event.shiftKey && !event.isComposing) {
			event.preventDefault();
			send();
		}
	}
</script>

<section class="agent-talk" aria-label="Ask the agent">
	{#each talk.entries as entry, index (index)}
		<article class="entry" class:mine={entry.from === 'you'}>
			<div class="meta">
				{#if entry.from === 'you'}
					<strong>You</strong>
					{#if entry.about !== null}<span class="about">{entry.about}</span>{/if}
				{:else}
					<span class="agent-mark"><SparklesIcon size={12} /></span>
					<strong>Agent</strong>
					{#if talk.waiting && index === talk.entries.length - 1}<span class="about">still working</span>{/if}
				{/if}
			</div>
			<!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions (delegated link clicks; the links themselves are focusable) -->
			<div
				class="markdown-body body"
				use:githubMarkdown={{ text: entry.text, repoUrl }}
				onclick={openMarkdownLinks}
			></div>
		</article>
	{/each}
	{#if talk.waiting && talk.entries.at(-1)?.from === 'you'}
		<div class="waiting"><SpinnerIcon size={12} /> The agent is looking…</div>
	{/if}

	<div class="composer">
		{#if about !== null}
			<div class="chip">
				<span>{aboutLabel(about)}</span>
				<button type="button" title="Ask about the pull request instead" onclick={onClearAbout}>×</button>
			</div>
		{/if}
		<textarea
			bind:this={box}
			bind:value={draft}
			rows="2"
			placeholder={unavailable ?? 'Ask the agent about this pull request, a comment ("Ask" on it) or the diff…'}
			disabled={unavailable !== null}
			onkeydown={onKeydown}
		></textarea>
		<div class="actions">
			<span class="hint">Enter to ask, Shift+Enter for a new line. Nothing is posted to GitHub.</span>
			<button
				type="button"
				class="btn primary"
				onclick={send}
				disabled={unavailable !== null || starting || draft.trim() === ''}
			>
				{starting ? 'Starting the agent…' : 'Ask'}
			</button>
		</div>
	</div>
</section>

<style>
	.agent-talk {
		display: flex;
		flex-direction: column;
		gap: var(--m-space-2);
		margin: 0 0 var(--m-space-3);
		font-size: var(--m-text-md);
	}
	.entry {
		padding: var(--m-space-2) var(--m-space-3);
		border: 1px solid var(--m-border);
		border-radius: var(--m-radius-md);
		background: var(--m-bg-1);
	}
	.entry.mine {
		background: var(--m-bg);
	}
	.meta {
		display: flex;
		align-items: center;
		gap: var(--m-space-2);
		font-size: var(--m-text-sm);
	}
	.agent-mark {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		width: 20px;
		height: 20px;
		border-radius: 50%;
		background: var(--m-accent-tint);
		color: var(--m-accent);
	}
	.about,
	.hint,
	.waiting {
		color: var(--m-fg-subtle);
		font-size: var(--m-text-sm);
	}
	.body {
		margin-top: var(--m-space-2);
		max-width: 860px;
		overflow-wrap: anywhere;
	}
	.waiting {
		display: flex;
		align-items: center;
		gap: var(--m-space-2);
		padding: 0 var(--m-space-3);
	}
	.composer {
		display: flex;
		flex-direction: column;
		gap: var(--m-space-2);
		padding: var(--m-space-2) var(--m-space-3);
		border: 1px solid var(--m-border);
		border-radius: var(--m-radius-md);
		background: var(--m-bg-1);
	}
	.chip {
		display: inline-flex;
		align-self: flex-start;
		align-items: center;
		gap: var(--m-space-1);
		padding: 0 0 0 var(--m-space-2);
		border-radius: var(--m-radius-pill);
		background: var(--m-accent-tint);
		color: var(--m-accent);
		font-size: var(--m-text-sm);
	}
	.chip button {
		padding: 0 var(--m-space-2);
		border: none;
		background: transparent;
		color: inherit;
		font: inherit;
		cursor: pointer;
	}
	textarea {
		min-height: 44px;
		max-height: 30vh;
		padding: 6px 8px;
		border: 1px solid var(--m-border);
		border-radius: var(--m-radius-sm);
		background: var(--m-bg);
		color: var(--m-fg);
		font: inherit;
		resize: vertical;
	}
	textarea:focus {
		outline: none;
		border-color: var(--m-accent);
	}
	.actions {
		display: flex;
		align-items: center;
		gap: var(--m-space-2);
	}
	.hint {
		flex: 1;
	}
	.btn {
		padding: 3px 12px;
		border: 1px solid var(--m-border);
		border-radius: var(--m-radius-md);
		background: var(--m-bg-2);
		color: var(--m-fg);
		font: inherit;
		font-size: var(--m-text-sm);
		cursor: pointer;
	}
	.btn.primary {
		border-color: var(--m-accent);
		background: var(--m-accent);
		color: var(--m-bg);
	}
	.btn:disabled {
		opacity: 0.5;
		cursor: default;
	}
</style>
