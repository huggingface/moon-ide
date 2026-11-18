<script lang="ts">
	// State controls of a pull request (ADR 0120): ready / draft,
	// merge, close / reopen. Each is one `gh` call made for a click;
	// the merge asks once more first, since it can't be taken back.
	import { inbox } from '../inbox.svelte';
	import { ipc } from '../ipc';
	import { MERGE_LABELS, mergeOffer } from '../prActions';
	import { formatError, type PrAction, type PrMergeMethod, type PullRequestDetail } from '../protocol';

	type Props = {
		pr: PullRequestDetail;
		/** Re-read the pull request; resolves once the new state is in. */
		onChanged: () => void | Promise<void>;
	};
	let { pr, onChanged }: Props = $props();

	const offer = $derived(mergeOffer(pr));
	let chosen = $state<PrMergeMethod | null>(null);
	const method = $derived(
		chosen !== null && pr.mergeMethods.includes(chosen) ? chosen : (pr.mergeMethods[0] ?? 'squash'),
	);

	let confirming = $state(false);
	/** The action in flight, for its button's label. */
	let running = $state<PrAction['kind'] | null>(null);
	let error = $state('');

	async function run(action: PrAction) {
		if (running !== null) {
			return;
		}
		running = action.kind;
		error = '';
		try {
			await ipc.pullRequest.act(pr.url, action);
			confirming = false;
			// The buttons stay busy until the new state is on screen,
			// so the one just used can't be pressed twice.
			await onChanged();
			void inbox.refresh();
		} catch (err) {
			error = formatError(err);
		} finally {
			running = null;
		}
	}

	const busy = $derived(running !== null);
</script>

{#if pr.state !== 'merged'}
	<div class="actions">
		{#if confirming && offer.kind === 'merge'}
			<span class="ask">
				{MERGE_LABELS[method]} #{pr.number} into <code>{pr.baseRef}</code>?
			</span>
			<button
				class="btn primary"
				disabled={busy}
				onclick={() => void run({ kind: 'merge', method, headSha: pr.headSha })}
			>
				{running === 'merge' ? 'Merging…' : 'Confirm merge'}
			</button>
			<button class="btn" disabled={busy} onclick={() => (confirming = false)}>Cancel</button>
		{:else}
			{#if offer.kind === 'ready'}
				<button class="btn primary" disabled={busy} onclick={() => void run({ kind: 'ready' })}>
					{running === 'ready' ? 'Marking ready…' : 'Ready for review'}
				</button>
			{:else if offer.kind === 'merge'}
				<button class="btn primary" disabled={busy} onclick={() => (confirming = true)}>
					{MERGE_LABELS[method]}
				</button>
				{#if pr.mergeMethods.length > 1}
					<select
						class="method"
						aria-label="Merge method"
						disabled={busy}
						value={method}
						onchange={(event) => (chosen = event.currentTarget.value as PrMergeMethod)}
					>
						{#each pr.mergeMethods as option (option)}
							<option value={option}>{MERGE_LABELS[option]}</option>
						{/each}
					</select>
				{/if}
				{#if offer.caution !== null}
					<span class="note">{offer.caution}</span>
				{/if}
			{:else if offer.kind === 'blocked'}
				<button class="btn primary" disabled title={offer.reason}>{MERGE_LABELS[method]}</button>
				<span class="note">{offer.reason}</span>
			{/if}
			<span class="spacer"></span>
			{#if pr.state === 'open'}
				<button class="btn quiet" disabled={busy} onclick={() => void run({ kind: 'draft' })}>
					{running === 'draft' ? 'Converting…' : 'Convert to draft'}
				</button>
			{/if}
			{#if pr.state === 'closed'}
				<button class="btn" disabled={busy} onclick={() => void run({ kind: 'reopen' })}>
					{running === 'reopen' ? 'Reopening…' : 'Reopen'}
				</button>
			{:else}
				<button class="btn quiet" disabled={busy} onclick={() => void run({ kind: 'close' })}>
					{running === 'close' ? 'Closing…' : 'Close'}
				</button>
			{/if}
		{/if}
	</div>
	{#if error !== ''}
		<p class="error">{error}</p>
	{/if}
{/if}

<style>
	.actions {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: var(--m-space-2);
		margin-top: var(--m-space-3);
		min-height: 26px;
		font-size: var(--m-text-sm);
	}
	.spacer {
		flex: 1;
	}
	.ask {
		color: var(--m-fg);
	}
	.note {
		color: var(--m-fg-muted);
	}
	.error {
		margin: var(--m-space-2) 0 0;
		color: var(--m-danger);
		font-size: var(--m-text-sm);
		overflow-wrap: anywhere;
	}
	.btn {
		flex-shrink: 0;
		height: 26px;
		padding: 0 var(--m-space-3);
		border: 1px solid var(--m-border);
		border-radius: var(--m-radius-sm);
		background: var(--m-bg);
		color: var(--m-fg);
		font: inherit;
		font-size: var(--m-text-sm);
		font-weight: 600;
		white-space: nowrap;
		cursor: pointer;
	}
	.btn:hover:not(:disabled) {
		border-color: var(--m-fg-muted);
	}
	.btn:disabled {
		opacity: 0.5;
		cursor: not-allowed;
	}
	.btn.primary {
		background: var(--m-success);
		border-color: var(--m-success);
		color: var(--m-bg);
	}
	.btn.quiet {
		border-color: transparent;
		background: transparent;
		color: var(--m-fg-muted);
		font-weight: 400;
	}
	.btn.quiet:hover:not(:disabled) {
		border-color: var(--m-border);
		color: var(--m-fg);
	}
	.method {
		height: 26px;
		padding: 0 var(--m-space-1);
		border: 1px solid var(--m-border);
		border-radius: var(--m-radius-sm);
		background: var(--m-bg);
		color: var(--m-fg);
		font: inherit;
		font-size: var(--m-text-sm);
	}
</style>
