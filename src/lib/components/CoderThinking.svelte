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

	function onBodyScroll(): void {
		const el = bodyEl;
		if (!el) {
			return;
		}
		const distance = el.scrollHeight - el.scrollTop - el.clientHeight;
		pinned = distance <= PIN_THRESHOLD_PX;
	}

	// Snap to the bottom on every text change *if* the user is
	// pinned and the message is still streaming. Reading the
	// element synchronously after the text update is fine — the
	// effect runs after Svelte flushes the DOM, so `scrollHeight`
	// already reflects the newly-appended chunk.
	$effect(() => {
		const _trigger = text;
		void _trigger;
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
	/* Match the parent panel's `.thinking` styling — kept as a
	   sibling here because Svelte's `:global` would leak across
	   every `.thinking` instance, and we explicitly want the
	   parent's CSS to win when this component is dropped into the
	   coder panel today. The parent rules in `CoderPanel.svelte`
	   stay the source of truth; the duplicates below only cover
	   the standalone case (testbeds, future placements). */
	.thinking {
		font-size: 12px;
		background: var(--m-bg-overlay);
		border-radius: 6px;
		padding: 6px 10px;
		color: var(--m-fg-muted);
	}
	.thinking summary {
		cursor: pointer;
		font-size: 11px;
		text-transform: uppercase;
		letter-spacing: 0.04em;
		color: var(--m-fg-subtle);
		user-select: none;
	}
	.thinking[open] summary {
		margin-bottom: 4px;
		border-bottom: 1px solid var(--m-border);
		padding-bottom: 4px;
	}
	.thinking-body {
		font-size: 12px;
		line-height: 1.5;
		color: var(--m-fg-muted);
		max-height: 320px;
		overflow-y: auto;
	}
</style>
