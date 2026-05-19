<script lang="ts">
	// Dedicated settings modal for the workspace-scoped HF Hub
	// trace bucket. Surfaced from the coder panel header by its
	// own button (separate from the LLM model-settings modal) so
	// that trace-sync state is reachable without poking inside
	// model-picker chrome.
	//
	// Two shapes:
	//   - **Disconnected**: a single "Connect to Hugging Face" CTA
	//     that opens `HfBucketConnectModal` on top.
	//   - **Connected**: shows `<owner>/<name>` (deep link to the
	//     Hub), the autosync checkbox, and a Disconnect button.

	import { coder } from '../coder.svelte';
	import { workspace } from '../state.svelte';
	import { formatError, type HubUploadAllSummary } from '../protocol';
	import { onMount } from 'svelte';
	import HfBucketConnectModal from './HfBucketConnectModal.svelte';

	type Props = { onClose: () => void };
	let { onClose }: Props = $props();

	let connectOpen = $state(false);
	let actionError = $state<string | null>(null);
	let uploadingAll = $state(false);
	let lastUploadAll = $state<HubUploadAllSummary | null>(null);

	onMount(() => {
		void coder.loadHubBinding();
	});

	async function onToggleAutosync(e: Event) {
		actionError = null;
		const checked = (e.target as HTMLInputElement).checked;
		try {
			await coder.setHubAutosync(checked);
			workspace.flash(checked ? 'Autosync on — uploads after every turn.' : 'Autosync off.');
		} catch (err) {
			actionError = formatError(err);
		}
	}

	async function onUploadAll() {
		if (uploadingAll) {
			return;
		}
		actionError = null;
		uploadingAll = true;
		try {
			const summary = await coder.uploadAllSessionsToHub();
			lastUploadAll = summary;
			const total = summary.uploaded + summary.skipped + summary.failed.length;
			if (total === 0) {
				workspace.flash('No sessions to upload yet.');
			} else if (summary.failed.length === 0) {
				const parts = [`${summary.uploaded} uploaded`];
				if (summary.skipped > 0) {
					parts.push(`${summary.skipped} already up to date`);
				}
				workspace.flash(parts.join(', ') + '.');
			} else {
				workspace.flash(`Uploaded ${summary.uploaded}, ${summary.failed.length} failed — see modal for details.`);
			}
			// Refresh the binding so the per-session `uploaded`
			// markers we just bumped surface in the session-list
			// row decoration straight away (the panel only knows
			// about them via `coder.hubBucket.uploaded`).
			await coder.loadHubBinding();
		} catch (err) {
			actionError = formatError(err);
		} finally {
			uploadingAll = false;
		}
	}

	async function onDisconnect() {
		actionError = null;
		try {
			await coder.disconnectHubBucket();
		} catch (err) {
			actionError = formatError(err);
		}
	}

	function onBackdropClick(e: MouseEvent) {
		if (e.target === e.currentTarget) {
			onClose();
		}
	}

	function onKeydown(e: KeyboardEvent) {
		if (e.key === 'Escape') {
			e.stopPropagation();
			onClose();
		}
	}
</script>

<div
	class="overlay"
	role="dialog"
	aria-modal="true"
	aria-label="Hugging Face trace sync"
	tabindex="-1"
	onclick={onBackdropClick}
	onkeydown={onKeydown}
>
	<div class="card">
		<header>
			<h2>Hugging Face trace sync</h2>
			<button type="button" class="close" aria-label="Close" onclick={onClose}>×</button>
		</header>

		<p class="lede">
			Sessions land under <code>sessions/</code> as pi-mono JSONLs so the Hub renders them inline in its trace viewer. One
			bucket per workspace.
		</p>

		{#if coder.hubBucket}
			{@const bucket = coder.hubBucket}
			<div class="summary">
				<span class="target">
					<a href="https://huggingface.co/buckets/{bucket.namespace}/{bucket.name}" target="_blank" rel="noreferrer">
						{bucket.namespace}/{bucket.name}
					</a>
				</span>
				<span class="status connected">connected</span>
			</div>

			<label class="autosync-row">
				<input type="checkbox" checked={bucket.autosync} onchange={onToggleAutosync} />
				<span>Autosync after every turn</span>
			</label>

			<p class="hint">
				Manual <code>Upload</code> is always available on each session row.
				<button type="button" class="link" onclick={onUploadAll} disabled={uploadingAll}>
					{uploadingAll ? 'Uploading…' : 'Upload all now'}
				</button> pushes every session from every bound folder in one batch.
			</p>

			{#if lastUploadAll && lastUploadAll.failed.length > 0}
				<details class="failures-block" open>
					<summary>{lastUploadAll.failed.length} failed to upload</summary>
					<ul class="failures">
						{#each lastUploadAll.failed as failure (failure.session_id)}
							<li><code>{failure.session_id}</code>: {failure.error}</li>
						{/each}
					</ul>
				</details>
			{/if}

			{#if actionError}
				<p class="error" role="alert">{actionError}</p>
			{/if}

			<div class="actions">
				<button type="button" class="secondary" onclick={onClose}>Close</button>
				<button type="button" class="danger" onclick={onDisconnect}>Disconnect</button>
			</div>
		{:else}
			<p class="hint">
				No bucket bound for this workspace yet. Connect one to start pushing traces — autosync stays off until you flip
				it on.
			</p>

			{#if actionError}
				<p class="error" role="alert">{actionError}</p>
			{/if}

			<div class="actions">
				<button type="button" class="secondary" onclick={onClose}>Close</button>
				<button type="button" class="primary" onclick={() => (connectOpen = true)}>Connect to Hugging Face</button>
			</div>
		{/if}
	</div>
</div>

{#if connectOpen}
	<HfBucketConnectModal onClose={() => (connectOpen = false)} />
{/if}

<style>
	.overlay {
		position: fixed;
		inset: 0;
		background: rgba(0, 0, 0, 0.55);
		display: flex;
		align-items: center;
		justify-content: center;
		z-index: 50;
	}
	.card {
		background: var(--m-bg-1);
		border: 1px solid var(--m-border);
		border-radius: 8px;
		padding: 18px 22px 20px;
		max-width: 460px;
		width: calc(100% - 32px);
		display: flex;
		flex-direction: column;
		gap: 14px;
		box-shadow: 0 12px 36px rgba(0, 0, 0, 0.55);
	}
	header {
		display: flex;
		align-items: center;
		justify-content: space-between;
	}
	h2 {
		font-size: 14px;
		font-weight: 600;
		margin: 0;
		color: var(--m-fg);
	}
	.close {
		background: transparent;
		border: 0;
		color: var(--m-fg-muted);
		cursor: pointer;
		font-size: 18px;
		line-height: 1;
		padding: 0 4px;
	}
	.close:hover {
		color: var(--m-fg);
	}
	.lede {
		margin: 0;
		font-size: 12px;
		color: var(--m-fg-muted);
		line-height: 1.5;
	}
	.summary {
		display: flex;
		align-items: center;
		gap: 8px;
		font-size: 12px;
	}
	.target a {
		color: var(--m-fg);
		text-decoration: underline;
		font-family: var(--m-font-mono, ui-monospace, monospace);
	}
	.status {
		font-size: 10px;
		text-transform: uppercase;
		letter-spacing: 0.06em;
		padding: 1px 6px;
		border-radius: 3px;
		border: 1px solid var(--m-border);
	}
	.status.connected {
		color: var(--m-success, #38a169);
		border-color: var(--m-success, #38a169);
	}
	.autosync-row {
		display: flex;
		align-items: center;
		gap: 6px;
		font-size: 12px;
		color: var(--m-fg);
	}
	.hint {
		font-size: 11px;
		color: var(--m-fg-subtle);
		margin: 0;
		line-height: 1.5;
	}
	.link {
		background: transparent;
		border: 0;
		padding: 0;
		font: inherit;
		color: var(--m-accent);
		cursor: pointer;
		text-decoration: underline;
	}
	.link:hover:not(:disabled) {
		filter: brightness(1.15);
	}
	.link:disabled {
		cursor: progress;
		opacity: 0.7;
	}
	.failures-block {
		font-size: 11px;
		color: var(--m-danger);
		background: color-mix(in srgb, var(--m-danger) 10%, transparent);
		border-radius: 4px;
		padding: 6px 10px;
	}
	.failures-block summary {
		cursor: pointer;
		user-select: none;
	}
	.failures {
		margin: 6px 0 0;
		padding-left: 18px;
		max-height: 100px;
		overflow-y: auto;
	}
	.failures li {
		margin-bottom: 2px;
	}
	.error {
		font-size: 12px;
		color: var(--m-danger);
		background: color-mix(in srgb, var(--m-danger) 12%, transparent);
		border-radius: 4px;
		padding: 6px 8px;
		margin: 0;
	}
	.actions {
		display: flex;
		justify-content: flex-end;
		gap: 8px;
		margin-top: 4px;
	}
	.primary,
	.secondary,
	.danger {
		font: inherit;
		border: 0;
		border-radius: 4px;
		padding: 8px 14px;
		cursor: pointer;
	}
	.primary {
		background: var(--m-accent);
		color: #fff;
	}
	.primary:hover:not(:disabled) {
		filter: brightness(1.1);
	}
	.secondary {
		background: var(--m-bg-overlay);
		color: var(--m-fg);
		border: 1px solid var(--m-border);
	}
	.secondary:hover:not(:disabled) {
		border-color: var(--m-fg-muted);
	}
	.danger {
		background: transparent;
		color: var(--m-danger);
		border: 1px solid color-mix(in srgb, var(--m-danger) 50%, var(--m-border));
	}
	.danger:hover:not(:disabled) {
		background: color-mix(in srgb, var(--m-danger) 10%, transparent);
	}
	code {
		font-family: var(--m-font-mono, ui-monospace, monospace);
		font-size: 11px;
	}
</style>
