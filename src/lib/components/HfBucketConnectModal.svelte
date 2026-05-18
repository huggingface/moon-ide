<script lang="ts">
	// Connect modal for the workspace-scoped HF Hub trace bucket.
	//
	// One bucket per workspace, provisioned under the user's login
	// or one of their orgs (the dropdown is populated from the
	// cached OAuth identity, so opening is instant). The visible
	// surface intentionally mirrors `CoderConnectModal.svelte` for
	// visual consistency — overlay + card + close button + a
	// single primary action — but the body shape is its own.

	import { coder } from '../coder.svelte';
	import { workspace } from '../state.svelte';
	import { formatError, type HubNamespace } from '../protocol';
	import { onMount } from 'svelte';

	type Props = { onClose: () => void };
	let { onClose }: Props = $props();

	let namespaces = $state<HubNamespace[]>([]);
	let loadingNamespaces = $state(true);
	let listError = $state<string | null>(null);

	let selectedNamespace = $state<string>('');
	let name = $state<string>(defaultBucketName());
	// Private by default; the team prefers buckets stay private
	// unless someone explicitly opts in to sharing.
	let isPrivate = $state(true);
	let creating = $state(false);
	let createError = $state<string | null>(null);

	onMount(async () => {
		try {
			const list = await coder.listHubNamespaces();
			namespaces = list;
			const first = list[0];
			if (first && !selectedNamespace) {
				selectedNamespace = first.name;
			}
		} catch (err) {
			listError = formatError(err);
		} finally {
			loadingNamespaces = false;
		}
	});

	// Default to the workspace name, not the active folder's
	// basename: a single binding lives on `WorkspaceSession`, so
	// every folder in this workspace pushes into the same bucket.
	// Slugify the workspace label so it satisfies HF's repo-name
	// rule (alphanumerics + `.-_`) — "Hugging Face" becomes
	// `hugging-face`. Falls back to a generic stub for preboot
	// or a workspace whose name slugifies to the empty string.
	function defaultBucketName(): string {
		const slug = slugifyForRepoName(workspace.workspaceName ?? '');
		const base = slug.length > 0 ? slug : 'moon-ide';
		return `${base}-traces`;
	}

	function slugifyForRepoName(s: string): string {
		return s
			.toLowerCase()
			.replace(/[^a-z0-9._-]+/g, '-')
			.replace(/-{2,}/g, '-')
			.replace(/^[-._]+|[-._]+$/g, '');
	}

	function labelFor(ns: HubNamespace): string {
		return ns.kind === 'user' ? `${ns.name} (you)` : `${ns.name} (org)`;
	}

	const namePreview = $derived(selectedNamespace && name ? `Will create ${selectedNamespace}/${name}` : '');

	const REPO_NAME_RE = /^[A-Za-z0-9][A-Za-z0-9._-]*$/;
	const nameValid = $derived(REPO_NAME_RE.test(name) && name.length <= 96);

	async function onCreate() {
		if (!selectedNamespace || !nameValid || creating) {
			return;
		}
		creating = true;
		createError = null;
		try {
			await coder.createHubBucket(selectedNamespace, name, isPrivate);
			workspace.flash(`Bucket created at ${selectedNamespace}/${name}.`);
			onClose();
		} catch (err) {
			createError = formatError(err);
		} finally {
			creating = false;
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
	aria-label="Connect Hugging Face trace bucket"
	tabindex="-1"
	onclick={onBackdropClick}
	onkeydown={onKeydown}
>
	<div class="card">
		<header>
			<h2>Connect to Hugging Face</h2>
			<button type="button" class="close" aria-label="Close" onclick={onClose}>×</button>
		</header>

		<p class="lede">
			Provision one HF Hub bucket for the
			<strong>{workspace.workspaceName ?? 'current'}</strong> workspace. Every folder bound to this workspace pushes its coder
			sessions here as pi-mono JSONLs the Hub can render in its trace viewer.
		</p>

		{#if loadingNamespaces}
			<p class="hint">Loading namespaces…</p>
		{:else if listError}
			<p class="error" role="alert">{listError}</p>
		{:else}
			<label class="field">
				<span class="label">Owner</span>
				<select bind:value={selectedNamespace}>
					{#each namespaces as ns (ns.name)}
						<option value={ns.name}>{labelFor(ns)}</option>
					{/each}
				</select>
			</label>

			<label class="field">
				<span class="label">Name</span>
				<input
					type="text"
					bind:value={name}
					placeholder="my-workspace-traces"
					maxlength="96"
					autocomplete="off"
					spellcheck="false"
				/>
				<span class="hint name-preview" class:invalid={!nameValid && name.length > 0}>
					{#if name.length === 0}
						Choose a name (alphanumeric, <code>.-_</code>, max 96 chars).
					{:else if !nameValid}
						Invalid name. Use alphanumerics, <code>.</code>, <code>-</code>, <code>_</code>, and start with a letter or
						digit.
					{:else}
						{namePreview}
					{/if}
				</span>
			</label>

			<fieldset class="visibility">
				<legend>Visibility</legend>
				<label>
					<input type="radio" bind:group={isPrivate} value={true} />
					Private
					<span class="hint">— only you and the owning org can read.</span>
				</label>
				<label>
					<input type="radio" bind:group={isPrivate} value={false} />
					Public
					<span class="hint">— anyone on the Hub can read.</span>
				</label>
			</fieldset>

			{#if createError}
				<p class="error" role="alert">{createError}</p>
			{/if}

			<div class="actions">
				<button type="button" class="secondary" onclick={onClose} disabled={creating}>Cancel</button>
				<button
					type="button"
					class="primary"
					onclick={onCreate}
					disabled={creating || !nameValid || !selectedNamespace}
				>
					{creating ? 'Creating…' : 'Create bucket'}
				</button>
			</div>
		{/if}
	</div>
</div>

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
	.field {
		display: flex;
		flex-direction: column;
		gap: 4px;
	}
	.label {
		font-size: 11px;
		color: var(--m-fg-subtle);
		text-transform: uppercase;
		letter-spacing: 0.5px;
	}
	select,
	input[type='text'] {
		font: inherit;
		background: var(--m-bg-overlay);
		color: var(--m-fg);
		border: 1px solid var(--m-border);
		border-radius: 4px;
		padding: 6px 8px;
	}
	input[type='text']:focus,
	select:focus {
		outline: none;
		border-color: var(--m-accent);
	}
	.visibility {
		border: 1px solid var(--m-border);
		border-radius: 4px;
		padding: 8px 10px;
		display: flex;
		flex-direction: column;
		gap: 4px;
		margin: 0;
	}
	.visibility legend {
		font-size: 11px;
		color: var(--m-fg-subtle);
		padding: 0 4px;
		text-transform: uppercase;
		letter-spacing: 0.5px;
	}
	.visibility label {
		font-size: 12px;
		color: var(--m-fg);
		display: flex;
		align-items: center;
		gap: 6px;
	}
	.hint {
		font-size: 11px;
		color: var(--m-fg-subtle);
		margin: 0;
	}
	.name-preview.invalid {
		color: var(--m-danger);
	}
	.actions {
		display: flex;
		justify-content: flex-end;
		gap: 8px;
		margin-top: 4px;
	}
	.primary,
	.secondary {
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
	.primary:disabled {
		cursor: not-allowed;
		opacity: 0.6;
	}
	.secondary {
		background: var(--m-bg-overlay);
		color: var(--m-fg);
		border: 1px solid var(--m-border);
	}
	.secondary:hover:not(:disabled) {
		border-color: var(--m-fg-muted);
	}
	.error {
		font-size: 12px;
		color: var(--m-danger);
		background: color-mix(in srgb, var(--m-danger) 12%, transparent);
		border-radius: 4px;
		padding: 6px 8px;
		margin: 0;
	}
	code {
		font-family: var(--m-font-mono, ui-monospace, monospace);
		font-size: 11px;
	}
</style>
