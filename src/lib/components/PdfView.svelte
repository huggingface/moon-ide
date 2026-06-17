<script lang="ts">
	import type { OpenFile } from '../state.svelte';
	import type { PDFDocumentLoadingTask } from 'pdfjs-dist';
	// Vite resolves the `?url` suffix to the bundled worker asset; the
	// import plugin's resolver doesn't grok the query string, so it can't
	// see the default export — it's there.
	// eslint-disable-next-line import/default
	import workerUrl from 'pdfjs-dist/build/pdf.worker.min.mjs?url';

	// pdf.js is ~400 kB; PDFs are opened rarely, so load the library
	// lazily on first render to keep it out of the main bundle. WebKitGTK
	// (the Linux / container webview) ships no native PDF viewer, so an
	// <iframe>/<embed> would render blank — we rasterise to canvas instead,
	// off the main thread via the Vite-bundled worker.
	async function loadPdfjs() {
		const pdfjs = await import('pdfjs-dist');
		pdfjs.GlobalWorkerOptions.workerSrc = workerUrl;
		return pdfjs;
	}

	type Props = { file: OpenFile };
	let { file }: Props = $props();

	let container: HTMLDivElement;
	let status = $state<'loading' | 'ready' | 'error'>('loading');
	let errorMessage = $state('');
	let pageCount = $state(0);

	// Cap the backing-store resolution so a hi-DPI screen doesn't allocate
	// gigantic canvases for a long document.
	const MAX_SCALE = 2;

	async function render() {
		status = 'loading';
		errorMessage = '';
		pageCount = 0;
		container.replaceChildren();
		let task: PDFDocumentLoadingTask | null = null;
		try {
			const pdfjs = await loadPdfjs();
			// Fetch the bytes through the asset protocol rather than handing
			// pdf.js the URL: the asset server doesn't speak HTTP range
			// requests, which pdf.js's URL path assumes.
			const bytes = await fetch(file.previewUrl).then((r) => r.arrayBuffer());
			task = pdfjs.getDocument({ data: new Uint8Array(bytes) });
			const doc = await task.promise;
			pageCount = doc.numPages;

			const scale = Math.min(window.devicePixelRatio || 1, MAX_SCALE);
			for (let n = 1; n <= doc.numPages; n++) {
				const page = await doc.getPage(n);
				const viewport = page.getViewport({ scale });
				const canvas = document.createElement('canvas');
				const ctx = canvas.getContext('2d');
				if (ctx === null) {
					continue;
				}
				canvas.width = viewport.width;
				canvas.height = viewport.height;
				// Lay the page out at CSS pixels; the extra backing-store
				// resolution stays crisp when zoomed by the OS.
				canvas.style.width = `${viewport.width / scale}px`;
				canvas.style.height = `${viewport.height / scale}px`;
				canvas.className = 'page';
				container.appendChild(canvas);
				await page.render({ canvas, canvasContext: ctx, viewport }).promise;
			}
			status = 'ready';
		} catch (err) {
			errorMessage = err instanceof Error ? err.message : String(err);
			status = 'error';
		} finally {
			void task?.destroy();
		}
	}

	$effect(() => {
		render();
	});
</script>

<div class="viewer">
	<div class="canvas">
		<div class="pages" bind:this={container}></div>
		{#if status === 'loading'}
			<p class="hint">Rendering PDF…</p>
		{:else if status === 'error'}
			<p class="hint error">Could not render PDF: {errorMessage}</p>
		{/if}
	</div>
	<footer class="meta">
		<span class="name">{file.name}</span>
		{#if pageCount > 0}
			<span class="dim">{pageCount} {pageCount === 1 ? 'page' : 'pages'}</span>
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
		position: relative;
		display: flex;
		flex-direction: column;
		padding: 24px;
		background: var(--m-bg-1);
		overflow: auto;
	}
	.pages {
		display: flex;
		flex-direction: column;
		align-items: center;
		gap: 16px;
	}
	.canvas :global(.page) {
		max-width: 100%;
		box-shadow: 0 1px 6px rgb(0 0 0 / 0.3);
		background: white;
	}
	.hint {
		margin: auto;
		color: var(--m-fg-muted);
		font-size: 13px;
	}
	.hint.error {
		color: var(--m-danger, #e55);
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
