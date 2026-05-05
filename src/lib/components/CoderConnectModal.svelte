<script lang="ts">
	import { openUrl } from '@tauri-apps/plugin-opener';
	import { coder } from '../coder.svelte';

	const code = $derived(coder.deviceCode);
	const verifyUrl = $derived(code?.verification_uri_complete ?? code?.verification_uri ?? null);

	function close() {
		coder.closeModal();
	}

	async function copyCode() {
		const value = code?.user_code;
		if (!value) {
			return;
		}
		try {
			await navigator.clipboard.writeText(value);
		} catch {
			// Clipboard access can fail under test runners or
			// hardened webviews; the visible code is the source of
			// truth, so a copy failure is silent.
		}
	}

	async function openInBrowser() {
		if (verifyUrl === null) {
			return;
		}
		try {
			await openUrl(verifyUrl);
		} catch {
			// Falling back to no-op: the user can click the URL
			// rendered in the modal too.
		}
	}
</script>

<!-- The modal is rendered conditionally by the parent; we always
	 mount real content here. The connect flow is a hard interrupt
	 of the panel's empty state, so we don't try to share visuals
	 with the rest of the panel. -->
<div class="overlay" role="dialog" aria-modal="true" aria-label="Sign in with Hugging Face">
	<div class="card">
		<header>
			<h2>Sign in with Hugging Face</h2>
			<button type="button" class="close" aria-label="Close" onclick={close}>×</button>
		</header>
		<p class="lede">
			Open the verification page and enter the code below to grant moon-ide access to Inference Providers and your
			private buckets.
		</p>
		{#if code}
			<button type="button" class="code" onclick={copyCode} title="Copy code">
				{code.user_code}
			</button>
			<div class="actions">
				<button type="button" class="primary" onclick={openInBrowser} disabled={verifyUrl === null}>
					Open in browser
				</button>
				{#if verifyUrl}
					<a class="link" href={verifyUrl} target="_blank" rel="noreferrer">
						{verifyUrl}
					</a>
				{/if}
			</div>
			<p class="hint" class:waiting={coder.awaitingApproval}>
				{#if coder.awaitingApproval}
					Waiting for approval…
				{:else}
					Click "Open in browser" to start.
				{/if}
			</p>
		{:else}
			<p class="hint">Requesting a device code…</p>
		{/if}
		{#if coder.signInError}
			<p class="error" role="alert">{coder.signInError}</p>
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
		max-width: 440px;
		width: calc(100% - 32px);
		display: flex;
		flex-direction: column;
		gap: 12px;
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
	.code {
		font-family: var(--m-font-mono, ui-monospace, monospace);
		font-size: 28px;
		letter-spacing: 4px;
		text-align: center;
		padding: 12px 8px;
		background: var(--m-bg-overlay);
		border: 1px solid var(--m-border);
		border-radius: 6px;
		color: var(--m-fg);
		cursor: pointer;
	}
	.code:hover {
		border-color: var(--m-accent);
	}
	.actions {
		display: flex;
		flex-direction: column;
		gap: 8px;
	}
	.primary {
		font: inherit;
		background: var(--m-accent);
		color: #fff;
		border: 0;
		border-radius: 4px;
		padding: 8px 14px;
		cursor: pointer;
	}
	.primary:hover:not(:disabled) {
		filter: brightness(1.1);
	}
	.primary:disabled {
		cursor: not-allowed;
		opacity: 0.6;
	}
	.link {
		font-size: 11px;
		color: var(--m-fg-subtle);
		word-break: break-all;
	}
	.hint {
		font-size: 11px;
		color: var(--m-fg-subtle);
		margin: 0;
	}
	.hint.waiting {
		color: var(--m-fg-muted);
	}
	.error {
		font-size: 12px;
		color: var(--m-danger);
		background: color-mix(in srgb, var(--m-danger) 12%, transparent);
		border-radius: 4px;
		padding: 6px 8px;
		margin: 0;
	}
</style>
