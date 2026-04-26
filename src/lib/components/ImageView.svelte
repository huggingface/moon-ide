<script lang="ts">
	import type { OpenFile } from '../state.svelte';

	type Props = { file: OpenFile };
	let { file }: Props = $props();

	let naturalWidth = $state(0);
	let naturalHeight = $state(0);

	function onLoad(event: Event) {
		const img = event.currentTarget as HTMLImageElement;
		naturalWidth = img.naturalWidth;
		naturalHeight = img.naturalHeight;
	}
</script>

<div class="viewer">
	<div class="canvas">
		<img src={file.previewUrl} alt={file.name} onload={onLoad} />
	</div>
	<footer class="meta">
		<span class="name">{file.name}</span>
		{#if naturalWidth > 0}
			<span class="dim">{naturalWidth} × {naturalHeight}</span>
		{/if}
	</footer>
</div>

<style>
	.viewer {
		display: flex;
		flex-direction: column;
		flex: 1;
		min-width: 0;
		min-height: 0;
	}
	.canvas {
		flex: 1;
		min-height: 0;
		display: flex;
		align-items: center;
		justify-content: center;
		padding: 24px;
		background-image:
			linear-gradient(45deg, var(--m-bg-1) 25%, transparent 25%),
			linear-gradient(-45deg, var(--m-bg-1) 25%, transparent 25%),
			linear-gradient(45deg, transparent 75%, var(--m-bg-1) 75%),
			linear-gradient(-45deg, transparent 75%, var(--m-bg-1) 75%);
		background-size: 16px 16px;
		background-position:
			0 0,
			0 8px,
			8px -8px,
			-8px 0;
		overflow: auto;
	}
	.canvas img {
		max-width: 100%;
		max-height: 100%;
		object-fit: contain;
		image-rendering: auto;
	}
	.meta {
		display: flex;
		gap: 16px;
		padding: 6px 12px;
		border-top: 1px solid var(--m-border);
		font-size: 12px;
		color: var(--m-fg-muted);
		background: var(--m-bg-1);
	}
	.dim {
		color: var(--m-fg-subtle);
	}
</style>
