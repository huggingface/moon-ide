<script lang="ts">
	import { openUrl } from '@tauri-apps/plugin-opener';
	import type { Attachment } from 'svelte/attachments';
	import { toOriginalUrl } from '../browserBridge';
	import { browserFrameStatus, registerSlot } from '../browserFrames.svelte';
	import { ipc } from '../ipc';
	import { workspace } from '../state.svelte';

	// In-IDE browser tab (ADR 0088): the URL bar, plus a slot the tab's
	// iframe covers. The iframe itself lives in the app-level frame
	// layer (`BrowserFrames`, ADR 0105) so the page survives this view
	// being unmounted by a tab or folder switch.
	type Props = { path: string };
	let { path }: Props = $props();

	const tab = $derived(workspace.browserTabs[path]);
	const tabId = $derived(tab?.id ?? null);
	const status = $derived(tabId === null ? undefined : browserFrameStatus.get(tabId));
	const resolved = $derived(status !== undefined && 'src' in status ? status : null);
	const error = $derived(status !== undefined && 'error' in status ? status.error : null);
	// Writable derived: typing overrides it, a navigation (new
	// `tab.url`) resets it.
	let draft = $derived(tab?.url ?? '');

	const slot: Attachment<HTMLElement> = (node) => {
		if (tabId === null) {
			return;
		}
		return registerSlot(tabId, node);
	};

	function submit(event: SubmitEvent) {
		event.preventDefault();
		let next = draft.trim();
		if (next.length === 0 || tabId === null) {
			return;
		}
		if (!/^[a-z][a-z0-9+.-]*:\/\//i.test(next)) {
			next = `http://${next}`;
		}
		void ipc.browser.navigate(tabId, next);
	}

	function reload() {
		if (tabId !== null) {
			void ipc.browser.reload(tabId);
		}
	}

	// The page's current location, on the proxy origin the system
	// browser can reach too.
	function openExternally() {
		if (resolved === null || !tab) {
			return;
		}
		const proxied = new URL(resolved.src).origin;
		void openUrl(toOriginalUrl(tab.url, resolved.originalOrigin, proxied));
	}
</script>

<div class="browser">
	<form class="bar" onsubmit={submit}>
		<button type="button" class="icon" title="Reload" onclick={reload}>↻</button>
		<input class="url" bind:value={draft} spellcheck="false" autocomplete="off" aria-label="URL" />
		{#if tab?.inContainer}
			<span class="badge" title="Resolved inside the workspace container through an IDE proxy">container</span>
		{/if}
		<button
			type="button"
			class="icon"
			title="Open in system browser"
			disabled={resolved === null}
			onclick={openExternally}>↗</button
		>
	</form>
	{#if error !== null}
		<div class="frame">
			<p class="error">{error}</p>
		</div>
	{:else}
		<div class="frame" {@attach slot}></div>
	{/if}
</div>

<style>
	.browser {
		display: flex;
		flex-direction: column;
		flex: 1;
		min-width: 0;
		min-height: 0;
	}
	.bar {
		display: flex;
		align-items: center;
		gap: 6px;
		padding: 4px 8px;
		border-bottom: 1px solid var(--m-border);
		background: var(--m-bg-1);
	}
	.url {
		flex: 1;
		min-width: 0;
		padding: 3px 8px;
		font-family: var(--m-font-mono);
		font-size: 12px;
		color: var(--m-fg);
		background: var(--m-bg);
		border: 1px solid var(--m-border);
		border-radius: 4px;
	}
	.url:focus {
		outline: none;
		border-color: var(--m-accent);
	}
	.icon {
		padding: 2px 6px;
		font-size: 14px;
		line-height: 1;
		color: var(--m-fg-muted);
		background: transparent;
		border: none;
		border-radius: 4px;
		cursor: pointer;
	}
	.icon:hover:not(:disabled) {
		color: var(--m-fg);
		background: var(--m-bg-2);
	}
	.icon:disabled {
		opacity: 0.4;
		cursor: default;
	}
	.badge {
		font-size: 11px;
		color: var(--m-fg-subtle);
	}
	.frame {
		flex: 1;
		min-height: 0;
		display: flex;
		background: white;
	}
	.error {
		margin: auto;
		max-width: 60ch;
		font-size: 13px;
		color: var(--m-danger);
		text-align: center;
	}
</style>
