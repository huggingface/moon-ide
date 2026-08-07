<script lang="ts">
	import type { OpenFile } from '../state.svelte';

	type Props = { file: OpenFile };
	let { file }: Props = $props();

	let width = $state(0);
	let height = $state(0);
	let duration = $state(0);
	let failed = $state(false);

	function onMetadata(event: Event) {
		const video = event.currentTarget as HTMLVideoElement;
		width = video.videoWidth;
		height = video.videoHeight;
		duration = video.duration;
	}

	function formatDuration(seconds: number): string {
		const total = Math.round(seconds);
		const m = Math.floor(total / 60);
		const s = total % 60;
		return `${m}:${String(s).padStart(2, '0')}`;
	}
</script>

<div class="viewer">
	<div class="canvas">
		<!-- Re-key on `previewToken`: `WorkspaceState.refreshPreviewFile`
		     bumps it (and cache-busts the asset URL) when the watcher
		     sees the bytes change on disk. Same reasoning as ImageView. -->
		{#key file.previewToken}
			{#if failed}
				<p class="error">
					Can't decode this video — the webview lacks a codec for it (WebKitGTK needs the GStreamer plugins for H.264).
				</p>
			{:else}
				<!-- svelte-ignore a11y_media_has_caption -->
				<video
					src={file.previewUrl}
					controls
					preload="metadata"
					onloadedmetadata={onMetadata}
					onerror={() => (failed = true)}
				></video>
			{/if}
		{/key}
	</div>
	<footer class="meta">
		<span class="name">{file.name}</span>
		{#if width > 0}
			<span class="dim">{width} × {height}</span>
		{/if}
		{#if duration > 0 && Number.isFinite(duration)}
			<span class="dim">{formatDuration(duration)}</span>
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
		background: var(--m-bg-1);
		overflow: auto;
	}
	.canvas video {
		max-width: 100%;
		max-height: 100%;
		object-fit: contain;
	}
	.error {
		max-width: 48ch;
		font-size: 13px;
		color: var(--m-fg-muted);
		text-align: center;
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
