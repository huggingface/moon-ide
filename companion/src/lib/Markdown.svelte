<script lang="ts">
	import { onDestroy } from 'svelte';
	import MarkdownIt from 'markdown-it';

	// Safe by construction: `html: false` escapes raw HTML in the
	// source, so `{@html}` below only ever injects markdown-it's own
	// output. Links open in a new tab so navigation doesn't blow away
	// the PWA's in-memory session state.
	const md = new MarkdownIt({ html: false, linkify: true });
	const defaultLink =
		md.renderer.rules.link_open ?? ((tokens, idx, options, _env, self) => self.renderToken(tokens, idx, options));
	md.renderer.rules.link_open = (tokens, idx, options, env, self) => {
		tokens[idx]?.attrSet('target', '_blank');
		tokens[idx]?.attrSet('rel', 'noopener noreferrer');
		return defaultLink(tokens, idx, options, env, self);
	};

	type Props = { text: string };
	let { text }: Props = $props();

	// rAF-coalesced render: streaming deltas arrive faster than
	// frames, and re-parsing + rewriting innerHTML per delta would
	// jank a phone. One parse per frame, always of the latest text.
	// (The desktop goes further with per-block caching — see
	// `src/lib/markdown.ts`; phone-scale messages don't need it.)
	let html = $state('');
	let pending = '';
	let frame: number | null = null;

	$effect(() => {
		pending = text;
		if (frame !== null) {
			return;
		}
		frame = requestAnimationFrame(() => {
			frame = null;
			html = md.render(pending);
		});
	});

	onDestroy(() => {
		if (frame !== null) {
			cancelAnimationFrame(frame);
		}
	});
</script>

<div class="md">
	<!-- eslint-disable-next-line svelte/no-at-html-tags -- markdown-it output with html:false, see above -->
	{@html html}
</div>

<style>
	.md :global(> :first-child) {
		margin-top: 0;
	}
	.md :global(> :last-child) {
		margin-bottom: 0;
	}
	.md :global(p),
	.md :global(ul),
	.md :global(ol),
	.md :global(pre),
	.md :global(blockquote) {
		margin: 0.4rem 0;
	}
	.md :global(h1),
	.md :global(h2),
	.md :global(h3),
	.md :global(h4) {
		margin: 0.6rem 0 0.3rem;
		font-size: 1rem;
	}
	.md :global(ul),
	.md :global(ol) {
		padding-left: 1.2rem;
	}
	.md :global(code) {
		font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
		font-size: 0.85em;
		background: var(--bg-elev-2);
		border-radius: 4px;
		padding: 0.1em 0.3em;
	}
	.md :global(pre) {
		background: var(--bg-elev-2);
		border: 1px solid var(--border);
		border-radius: var(--radius);
		padding: 0.5rem;
		overflow-x: auto;
	}
	.md :global(pre code) {
		background: none;
		padding: 0;
		font-size: 0.75rem;
	}
	.md :global(blockquote) {
		border-left: 3px solid var(--border);
		padding-left: 0.6rem;
		color: var(--fg-muted);
	}
	.md :global(table) {
		border-collapse: collapse;
		display: block;
		overflow-x: auto;
		font-size: 0.85em;
	}
	.md :global(th),
	.md :global(td) {
		border: 1px solid var(--border);
		padding: 0.2rem 0.5rem;
	}
	.md :global(hr) {
		border: none;
		border-top: 1px solid var(--border);
	}
</style>
