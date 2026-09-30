<script lang="ts">
	import { openUrl } from '@tauri-apps/plugin-opener';
	import { untrack } from 'svelte';
	import type { Attachment } from 'svelte/attachments';
	import { handleFrameMessage, registerFrame, toOriginalUrl } from '../browserBridge';
	import { ipc } from '../ipc';
	import { workspace } from '../state.svelte';

	// In-IDE browser tab (ADR 0088). An iframe on a preview-proxy URL;
	// the proxy injects the page bridge, which this view relays to the
	// coder (`browserBridge.ts`) and which reports in-page navigation
	// back into the tab's URL.
	type Props = { path: string };
	let { path }: Props = $props();

	const tab = $derived(workspace.browserTabs[path]);
	// Primitive deriveds so the resolve effect only re-runs on a real
	// reload / side switch, not on every tab-object replacement (a
	// location report replaces the object without wanting a reload).
	const tabId = $derived(tab?.id ?? null);
	const reloadToken = $derived(tab?.reloadToken ?? 0);
	const inContainer = $derived(tab?.inContainer ?? false);

	let resolved = $state<{ src: string; originalOrigin: string } | null>(null);
	let error = $state<string | null>(null);
	// Writable derived: typing overrides it, a navigation (new
	// `tab.url`) resets it.
	let draft = $derived(tab?.url ?? '');

	// Resolution is async IPC (it may start a proxy), so it can't be a
	// `$derived`. The `cancelled` guard drops a stale answer when the
	// user navigates again before the previous one lands.
	$effect(() => {
		if (tabId === null) {
			return;
		}
		void reloadToken;
		const side = inContainer;
		const url = untrack(() => tab?.url ?? '');
		let cancelled = false;
		error = null;
		ipc.browser
			.resolveUrl(url, side)
			.then((src) => {
				if (!cancelled) {
					resolved = { src, originalOrigin: new URL(url).origin };
				}
			})
			.catch((err: unknown) => {
				if (!cancelled) {
					resolved = null;
					error = err instanceof Error ? err.message : String(err);
				}
			});
		return () => {
			cancelled = true;
		};
	});

	const frameBridge: Attachment<HTMLIFrameElement> = (node) => {
		const target = node.contentWindow;
		if (tabId === null || resolved === null || target === null) {
			return;
		}
		const id = tabId;
		const unregister = registerFrame(id, {
			target,
			proxiedOrigin: new URL(resolved.src).origin,
			originalOrigin: resolved.originalOrigin,
		});
		const onMessage = (event: MessageEvent) => {
			if (event.source === target) {
				handleFrameMessage(
					id,
					event.data,
					untrack(() => tab?.url ?? ''),
				);
			}
		};
		window.addEventListener('message', onMessage);
		return () => {
			window.removeEventListener('message', onMessage);
			unregister();
		};
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
	<div class="frame">
		{#if error !== null}
			<p class="error">{error}</p>
		{:else if resolved !== null && tab}
			{#key `${resolved.src}#${reloadToken}`}
				<iframe src={resolved.src} title={tab.url} {@attach frameBridge}></iframe>
			{/key}
		{/if}
	</div>
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
	iframe {
		flex: 1;
		border: none;
	}
	.error {
		margin: auto;
		max-width: 60ch;
		font-size: 13px;
		color: var(--m-danger);
		text-align: center;
	}
</style>
