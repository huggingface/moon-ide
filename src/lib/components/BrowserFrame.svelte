<script lang="ts">
	import { untrack } from 'svelte';
	import type { Attachment } from 'svelte/attachments';
	import { handleFrameMessage, registerFrame } from '../browserBridge';
	import { browserCapture, browserFrameStatus, browserSlots } from '../browserFrames.svelte';
	import { ipc } from '../ipc';
	import type { BrowserTabView } from '../state.svelte';

	// One browser tab's iframe, kept alive for the tab's lifetime
	// (ADR 0105). While a `BrowserView` shows the tab, the frame covers
	// that view's slot; otherwise it stays loaded but hidden, at the size
	// it was last shown, so the page keeps its state and the coder can
	// drive it without pulling the tab to the front.
	type Props = { tab: BrowserTabView };
	let { tab }: Props = $props();

	const tabId = $derived(tab.id);
	const reloadToken = $derived(tab.reloadToken);
	const inContainer = $derived(tab.inContainer);
	const status = $derived(browserFrameStatus.get(tabId));
	const resolved = $derived(status !== undefined && 'src' in status ? status : null);

	type Box = { left: number; top: number; width: number; height: number };
	let shown = $state<Box | null>(null);
	// Size the page lays out at while hidden; a sensible editor-ish
	// default until the tab has been on screen once.
	let lastSize = $state({ width: 1280, height: 800 });
	let viewportWidth = $state(0);
	let viewportHeight = $state(0);

	$effect(() => {
		const id = tabId;
		void reloadToken;
		const side = inContainer;
		const url = untrack(() => tab.url);
		let cancelled = false;
		ipc.browser
			.resolveUrl(url, side)
			.then((src) => {
				if (!cancelled) {
					browserFrameStatus.set(id, { src, originalOrigin: new URL(url).origin });
				}
			})
			.catch((err: unknown) => {
				if (!cancelled) {
					browserFrameStatus.set(id, { error: err instanceof Error ? err.message : String(err) });
				}
			});
		return () => {
			cancelled = true;
		};
	});

	$effect(() => {
		const id = tabId;
		return () => {
			browserFrameStatus.delete(id);
		};
	});

	// Follow the slot every frame while it's mounted: split resizes,
	// panel toggles and sidebar drags move it without resizing it, so
	// a ResizeObserver alone would miss them.
	$effect(() => {
		const slot = browserSlots.get(tabId);
		if (slot === undefined) {
			shown = null;
			return;
		}
		let raf = 0;
		const follow = () => {
			const r = slot.getBoundingClientRect();
			const prev = untrack(() => shown);
			if (
				prev === null ||
				prev.left !== r.left ||
				prev.top !== r.top ||
				prev.width !== r.width ||
				prev.height !== r.height
			) {
				shown = { left: r.left, top: r.top, width: r.width, height: r.height };
				if (r.width > 0 && r.height > 0) {
					lastSize = { width: r.width, height: r.height };
				}
			}
			raf = requestAnimationFrame(follow);
		};
		follow();
		return () => {
			cancelAnimationFrame(raf);
		};
	});

	const visible = $derived(shown !== null && shown.width > 0 && shown.height > 0);
	const capturing = $derived(browserCapture.tabId === tabId);
	// Hidden frames sit at the top-left at their last size, so a
	// capture shows them without a relayout.
	const box = $derived<Box>(
		visible && shown !== null ? shown : { left: 0, top: 0, width: lastSize.width, height: lastSize.height },
	);

	const frameBridge: Attachment<HTMLIFrameElement> = (node) => {
		const target = node.contentWindow;
		if (resolved === null || target === null) {
			return;
		}
		const id = tabId;
		const unregister = registerFrame(id, {
			element: node,
			target,
			proxiedOrigin: new URL(resolved.src).origin,
			originalOrigin: resolved.originalOrigin,
		});
		const onMessage = (event: MessageEvent) => {
			if (event.source === target) {
				handleFrameMessage(
					id,
					event.data,
					untrack(() => tab.url),
				);
			}
		};
		window.addEventListener('message', onMessage);
		return () => {
			window.removeEventListener('message', onMessage);
			unregister();
		};
	};
</script>

<svelte:window bind:innerWidth={viewportWidth} bind:innerHeight={viewportHeight} />

{#if resolved !== null}
	{#key `${resolved.src}#${reloadToken}`}
		<iframe
			src={resolved.src}
			title={tab.url}
			class:hidden={!visible && !capturing}
			class:capturing
			style:left="{box.left}px"
			style:top="{box.top}px"
			style:width="{Math.min(box.width, viewportWidth)}px"
			style:height="{Math.min(box.height, viewportHeight)}px"
			{@attach frameBridge}
		></iframe>
	{/key}
{/if}

<style>
	/* Above the editor area's plain content, below every overlay
	   (menus, palettes, modals and tooltips all use higher z-indexes). */
	iframe {
		position: fixed;
		z-index: 1;
		border: none;
		background: white;
	}
	iframe.hidden {
		visibility: hidden;
		pointer-events: none;
	}
	iframe.capturing {
		z-index: 2147483000;
		pointer-events: none;
	}
</style>
