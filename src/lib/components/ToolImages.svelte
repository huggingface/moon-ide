<script lang="ts">
	import { parseToolImages } from './toolBodyHelpers';

	interface Props {
		result: unknown;
	}

	let { result }: Props = $props();

	const images = $derived(parseToolImages(result));

	// Local lightbox rather than the panel's user-image one: this
	// component also renders inside the sub-agent pop-out, which
	// is outside the panel's lightbox state.
	let lightboxUrl = $state<string | null>(null);
</script>

{#if images.length > 0}
	<div class="tool-images">
		{#each images as img, i (img.dataUrl.length + ':' + i)}
			<button type="button" class="tool-image" title="Open image full-size" onclick={() => (lightboxUrl = img.dataUrl)}>
				<img src={img.dataUrl} alt="{img.mime} attached by the tool ({i + 1})" />
			</button>
		{/each}
	</div>
{/if}

{#if lightboxUrl !== null}
	<!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions -->
	<div
		class="lightbox-backdrop"
		onclick={() => (lightboxUrl = null)}
		role="dialog"
		aria-modal="true"
		aria-label="Image preview"
		tabindex="-1"
	>
		<img class="lightbox-image" src={lightboxUrl} alt="Tool result at full size" />
		<button type="button" class="lightbox-close" title="Close" onclick={() => (lightboxUrl = null)}>×</button>
	</div>
{/if}

<style>
	.tool-images {
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
		margin-top: 4px;
	}
	.tool-image {
		padding: 0;
		border: 1px solid var(--m-border);
		border-radius: 4px;
		background: var(--m-bg);
		cursor: zoom-in;
		overflow: hidden;
	}
	.tool-image img {
		display: block;
		max-width: 240px;
		max-height: 160px;
		object-fit: contain;
	}
	/* Mirrors the panel's user-image lightbox (full-viewport dim,
	   centred image). Duplicated rather than shared because the
	   two live in different components with separate state. */
	.lightbox-backdrop {
		position: fixed;
		inset: 0;
		z-index: 90;
		display: flex;
		align-items: center;
		justify-content: center;
		background: rgba(0, 0, 0, 0.72);
	}
	.lightbox-image {
		max-width: 92vw;
		max-height: 92vh;
		object-fit: contain;
		border-radius: 4px;
	}
	.lightbox-close {
		position: absolute;
		top: 12px;
		right: 16px;
		border: none;
		background: transparent;
		color: #fff;
		font-size: 24px;
		cursor: pointer;
	}
</style>
