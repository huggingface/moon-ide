<script lang="ts">
	import { normalizeBrowserUrl, openInIdeBrowser } from '../browserOpen';
	import { palette } from '../commands.svelte';
	import { formatError } from '../protocol';
	import { workspace } from '../state.svelte';
	import { workspacePicker } from '../workspacePicker.svelte';
	import { workspaceCreate } from '../workspaceCreate.svelte';

	type Props = {
		onPickFolder: () => void | Promise<void>;
	};
	let { onPickFolder }: Props = $props();

	// Shortcut hints adapt to the platform — `⌘` on macOS,
	// `Ctrl` everywhere else. Plain string compare against the
	// UA is the cheapest reliable signal in a webview.
	const isMac =
		typeof navigator !== 'undefined' && /Mac|iPhone|iPad|iPod/.test(navigator.platform || navigator.userAgent);
	const mod = isMac ? '⌘' : 'Ctrl';

	// With folders bound this is just the "no file open" pane.
	const hasFolders = $derived((workspace.workspace?.folders.length ?? 0) > 0);

	let browserUrl = $state('');

	function openBrowserTab(event: SubmitEvent): void {
		event.preventDefault();
		const url = normalizeBrowserUrl(browserUrl);
		if (url === null) {
			workspace.flash('Enter an http(s) address, e.g. localhost:5173');
			return;
		}
		openInIdeBrowser(url).then(
			() => {
				browserUrl = '';
			},
			(err: unknown) => workspace.flash(`Couldn't open the tab: ${formatError(err)}`),
		);
	}
</script>

<div class="welcome">
	<div class="card">
		<h1>{workspace.workspaceName ?? 'moon-ide'}</h1>
		{#if hasFolders}
			<p class="subtitle">No file open.</p>
			<form class="browser-open" onsubmit={openBrowserTab}>
				<input
					type="text"
					placeholder="localhost:5173 or any URL"
					aria-label="Address to open in a browser tab"
					spellcheck="false"
					bind:value={browserUrl}
				/>
				<button type="submit" class="primary">Open in browser tab</button>
			</form>
		{:else}
			<p class="subtitle">This workspace is empty. Open a folder to begin.</p>
			<div class="actions">
				<button class="primary" onclick={() => void onPickFolder()}>Add folder…</button>
			</div>
		{/if}
		<div class="shortcuts" aria-label="Workspace shortcuts">
			{#if hasFolders}
				<div class="shortcut">
					<kbd>{mod}+P</kbd>
					<button type="button" class="link" onclick={() => palette.show('files')}>Go to file</button>
				</div>
			{/if}
			<div class="shortcut">
				<kbd>{mod}+Shift+O</kbd>
				<button type="button" class="link" onclick={() => void workspacePicker.open()}>Switch workspace</button>
			</div>
			<div class="shortcut">
				<kbd>{mod}+Shift+N</kbd>
				<button type="button" class="link" onclick={() => workspaceCreate.open()}>New workspace</button>
			</div>
			<div class="shortcut">
				<kbd>{mod}+Shift+A</kbd>
				<span class="link-static">Add folder to this workspace</span>
			</div>
		</div>
	</div>
</div>

<style>
	.welcome {
		flex: 1;
		display: flex;
		align-items: center;
		justify-content: center;
		padding: 32px;
	}
	.card {
		text-align: center;
		max-width: 460px;
		display: flex;
		flex-direction: column;
		align-items: center;
		gap: var(--m-space-3);
	}
	h1 {
		margin: 0;
		font-size: 22px;
		font-weight: 600;
		letter-spacing: -0.01em;
	}
	.subtitle {
		margin: 0;
		color: var(--m-fg-muted);
		font-size: var(--m-text-md);
	}
	.actions {
		display: flex;
		gap: var(--m-space-2);
		margin-top: var(--m-space-2);
	}
	.browser-open {
		display: flex;
		gap: var(--m-space-2);
		width: 100%;
		margin-top: var(--m-space-2);
	}
	.browser-open input {
		flex: 1;
		min-width: 0;
		font: inherit;
		font-size: var(--m-text-md);
		color: var(--m-fg);
		background: var(--m-bg-2);
		border: 1px solid var(--m-border);
		border-radius: var(--m-radius-md);
		padding: 0 var(--m-space-3);
		height: 32px;
	}
	.browser-open input:focus {
		outline: none;
		border-color: var(--m-accent);
	}
	.primary {
		background: var(--m-accent);
		color: var(--m-bg);
		font-weight: 600;
		border-radius: var(--m-radius-md);
		padding: 0 var(--m-space-4);
		border: 0;
		font-family: inherit;
		cursor: pointer;
		height: 32px;
		font-size: var(--m-text-md);
	}
	.primary:hover {
		background: var(--m-accent-strong);
	}
	.shortcuts {
		display: flex;
		flex-direction: column;
		align-items: stretch;
		gap: 1px;
		font-size: var(--m-text-sm);
		color: var(--m-fg-muted);
		margin-top: var(--m-space-4);
		min-width: 280px;
	}
	.shortcut {
		display: flex;
		align-items: center;
		gap: var(--m-space-3);
		padding: 0 var(--m-space-2);
		border-radius: var(--m-radius-md);
		height: 30px;
	}
	.shortcut:hover {
		background: var(--m-bg-1);
	}
	kbd {
		font-family: var(--m-font-mono);
		font-size: var(--m-text-xs);
		background: transparent;
		padding: 0;
		color: var(--m-fg-subtle);
		min-width: 96px;
		text-align: right;
		order: 1;
	}
	.link {
		flex: 1;
		text-align: left;
		background: transparent;
		border: 0;
		color: var(--m-fg-muted);
		font-family: inherit;
		font-size: var(--m-text-sm);
		cursor: pointer;
		padding: 0;
	}
	.link:hover {
		color: var(--m-accent);
	}
	.link-static {
		flex: 1;
		text-align: left;
		color: var(--m-fg-muted);
	}
</style>
