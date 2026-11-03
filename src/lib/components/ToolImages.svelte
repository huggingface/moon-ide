<script lang="ts">
	import { openLightbox } from '../imageLightbox.svelte';
	import { parseToolImages } from './toolBodyHelpers';

	interface Props {
		result: unknown;
	}

	let { result }: Props = $props();

	const images = $derived(parseToolImages(result));
</script>

{#if images.length > 0}
	<div class="tool-images">
		{#each images as img, i (img.dataUrl.length + ':' + i)}
			<button
				type="button"
				class="tool-image"
				title="Open image full-size"
				onclick={() => openLightbox({ src: img.dataUrl, alt: `${img.mime} attached by the tool (${i + 1})` })}
			>
				<img src={img.dataUrl} alt="{img.mime} attached by the tool ({i + 1})" />
			</button>
		{/each}
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
</style>
