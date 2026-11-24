<script lang="ts">
	import CoderMarkdown from './CoderMarkdown.svelte';

	type Props = {
		text: string;
		open: boolean;
		onOpenChange: (next: boolean) => void;
		// Streaming hint — when `false` (message finished) we stop
		// pinning to the bottom on every text change. The user is
		// presumably reading at this point and re-snapping the
		// scrollbar would feel jumpy.
		streaming: boolean;
	};
	let { text, open, onOpenChange, streaming }: Props = $props();

	let bodyEl = $state<HTMLDivElement>();
	// `pinned` flips off the moment the user scrolls away from the
	// bottom and back on if they scroll back to it. Same gesture
	// gmail / slack threads use: read freely from the top, snap to
	// new content from the bottom. Threshold is "within 24 px" to
	// absorb subpixel scroll positions on HiDPI.
	const PIN_THRESHOLD_PX = 24;
	let pinned = $state(true);
	let lastTop = 0;

	// Unpinned only by a move up: our own scroll-to-bottom's event can
	// land after the next chunk rendered, far from the new bottom.
	function onBodyScroll(): void {
		const el = bodyEl;
		if (!el) {
			return;
		}
		const distance = el.scrollHeight - el.scrollTop - el.clientHeight;
		if (distance <= PIN_THRESHOLD_PX) {
			pinned = true;
		} else if (el.scrollTop < lastTop) {
			pinned = false;
		}
		lastTop = el.scrollTop;
	}

	// The text renders as markdown a frame after it arrives, so its
	// growth is followed where it happens.
	$effect(() => {
		const el = bodyEl;
		const content = el?.firstElementChild;
		if (!el || !content || !streaming) {
			return;
		}
		const follow = new ResizeObserver(() => {
			if (pinned) {
				el.scrollTop = el.scrollHeight;
			}
		});
		follow.observe(content);
		return () => follow.disconnect();
	});

	// Snap to the bottom on every text change *if* the user is
	// pinned and the message is still streaming. Reading the
	// element synchronously after the text update is fine — the
	// effect runs after Svelte flushes the DOM, so `scrollHeight`
	// already reflects the newly-appended chunk.
	$effect(() => {
		// `void text` reads the reactive chunk list so the effect
		// re-runs on append; the actual work below doesn't.
		void text;
		if (!streaming) {
			return;
		}
		const el = bodyEl;
		if (!el || !pinned) {
			return;
		}
		el.scrollTop = el.scrollHeight;
	});
</script>

<details class="thinking" {open} ontoggle={(event) => onOpenChange((event.target as HTMLDetailsElement).open)}>
	<summary>thinking{streaming ? '…' : ''}</summary>
	<div class="thinking-body" bind:this={bodyEl} onscroll={onBodyScroll}>
		<CoderMarkdown {text} />
	</div>
</details>

<style>
	/* The reasoning trace reads as an aside: a quiet disclosure line,
	   and when open, muted text set off by a rule on the left instead
	   of a box. */
	.thinking {
		font-size: var(--m-text-sm);
		color: var(--m-fg-muted);
	}
	.thinking summary {
		cursor: pointer;
		font-size: var(--m-text-sm);
		color: var(--m-fg-subtle);
		user-select: none;
		line-height: 22px;
		width: fit-content;
	}
	.thinking summary::first-letter {
		text-transform: uppercase;
	}
	.thinking summary::marker {
		font-size: 10px;
	}
	.thinking summary:hover {
		color: var(--m-fg-muted);
	}
	.thinking[open] summary {
		margin-bottom: var(--m-space-1);
	}
	.thinking-body {
		font-size: var(--m-text-md);
		line-height: 1.5;
		color: var(--m-fg-muted);
		max-height: 320px;
		overflow-y: auto;
		margin-left: 3px;
		padding-left: var(--m-space-3);
		border-left: 2px solid var(--m-border);
		margin-bottom: var(--m-space-1);
	}
</style>
