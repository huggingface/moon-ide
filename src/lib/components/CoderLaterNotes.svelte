<script lang="ts">
	// Messages the user parked in this session to send later (the
	// composer's Later, Alt+Enter), above the composer. Kept with the
	// session and never part of what the model sees until one is
	// sent. Send puts it through as a message (a steer while a turn
	// runs); Edit moves it back into the composer.
	import { coder } from '../coder.svelte';
	import type { SessionNote } from '../protocol';
	import CloseIcon from './icons/CloseIcon.svelte';

	const notes = $derived(coder.notes);
	let open = $state(true);

	function without(note: SessionNote): SessionNote[] {
		return coder.notes.filter((one) => one.id !== note.id);
	}

	/** The note, then whatever was already typed. */
	function intoComposer(note: SessionNote): void {
		const typed = coder.draft.trim();
		coder.draft = typed === '' ? note.text : `${note.text}\n\n${typed}`;
	}

	async function send(note: SessionNote): Promise<void> {
		intoComposer(note);
		await coder.setNotes(without(note));
		await coder.send();
	}

	async function edit(note: SessionNote): Promise<void> {
		intoComposer(note);
		await coder.setNotes(without(note));
	}
</script>

{#if notes.length > 0}
	<div class="later">
		<button type="button" class="later-head" aria-expanded={open} onclick={() => (open = !open)}>
			Later <span class="later-count">{notes.length}</span>
		</button>
		{#if open}
			<ul class="later-list">
				{#each notes as note (note.id)}
					<li class="later-note">
						<span class="later-text" title={note.text}>{note.text}</span>
						<button type="button" class="later-btn" title="Send it now" onclick={() => void send(note)}>Send</button>
						<button type="button" class="later-btn" title="Move it into the composer" onclick={() => void edit(note)}
							>Edit</button
						>
						<button
							type="button"
							class="later-btn icon"
							title="Delete"
							aria-label="Delete note"
							onclick={() => void coder.setNotes(without(note))}
						>
							<CloseIcon size={11} />
						</button>
					</li>
				{/each}
			</ul>
		{/if}
	</div>
{/if}

<style>
	.later {
		display: flex;
		flex-direction: column;
		gap: 2px;
		min-width: 0;
		font-size: var(--m-text-sm);
	}
	.later-head {
		align-self: flex-start;
		display: inline-flex;
		align-items: center;
		gap: var(--m-space-1);
		padding: 0 var(--m-space-1);
		border: none;
		border-radius: var(--m-radius-sm);
		background: none;
		color: var(--m-fg-subtle);
		font: inherit;
		cursor: pointer;
	}
	.later-head:hover {
		color: var(--m-fg-muted);
	}
	.later-count {
		padding: 0 5px;
		border-radius: var(--m-radius-pill);
		background: var(--m-bg-3);
		font-size: var(--m-text-xs);
		font-variant-numeric: tabular-nums;
	}
	.later-list {
		display: flex;
		flex-direction: column;
		gap: 2px;
		max-height: 112px;
		margin: 0;
		padding: 0;
		overflow-y: auto;
		list-style: none;
	}
	.later-note {
		display: flex;
		align-items: center;
		gap: var(--m-space-1);
		min-width: 0;
		padding: 2px var(--m-space-1) 2px var(--m-space-2);
		border-radius: var(--m-radius-md);
		background: var(--m-bg-2);
	}
	.later-text {
		flex: 1;
		min-width: 0;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
		color: var(--m-fg-muted);
	}
	.later-btn {
		flex-shrink: 0;
		padding: 1px 6px;
		border: none;
		border-radius: var(--m-radius-sm);
		background: transparent;
		color: var(--m-fg-subtle);
		font: inherit;
		font-size: var(--m-text-xs);
		cursor: pointer;
	}
	.later-btn:hover {
		background: var(--m-bg-3);
		color: var(--m-fg);
	}
	.later-btn.icon {
		display: inline-flex;
		align-items: center;
		padding: 3px;
	}
</style>
