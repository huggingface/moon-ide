<script lang="ts">
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
</script>

<div class="welcome">
	<div class="card">
		<h1>{workspace.workspaceName ?? 'moon-ide'}</h1>
		<p class="subtitle">This workspace is empty. Open a folder to begin.</p>
		<div class="actions">
			<button class="primary" onclick={() => void onPickFolder()}>Add folder…</button>
		</div>
		<div class="shortcuts" aria-label="Workspace shortcuts">
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
		gap: 16px;
	}
	h1 {
		margin: 0;
		font-size: 28px;
		font-weight: 600;
		letter-spacing: -0.01em;
	}
	.subtitle {
		margin: 0;
		color: var(--m-fg-muted);
	}
	.actions {
		display: flex;
		gap: 8px;
	}
	.primary {
		background: var(--m-accent);
		color: #0d1017;
		font-weight: 600;
		border-radius: 6px;
		padding: 8px 18px;
		border: 0;
		font-family: inherit;
		cursor: pointer;
	}
	.primary:hover {
		background: var(--m-accent-strong);
	}
	.shortcuts {
		display: flex;
		flex-direction: column;
		align-items: stretch;
		gap: 6px;
		font-size: 12px;
		color: var(--m-fg-muted);
		margin-top: 16px;
		min-width: 280px;
	}
	.shortcut {
		display: flex;
		align-items: center;
		gap: 12px;
		padding: 6px 10px;
		border-radius: 4px;
	}
	.shortcut:hover {
		background: var(--m-bg-2);
	}
	kbd {
		font-family: var(--m-font-mono);
		font-size: 11px;
		background: var(--m-bg-2);
		border: 1px solid var(--m-border);
		border-radius: 3px;
		padding: 2px 6px;
		color: var(--m-fg);
		min-width: 88px;
		text-align: center;
	}
	.link {
		flex: 1;
		text-align: left;
		background: transparent;
		border: 0;
		color: var(--m-fg);
		font-family: inherit;
		font-size: 12px;
		cursor: pointer;
		padding: 0;
	}
	.link:hover {
		color: var(--m-accent);
	}
	.link-static {
		flex: 1;
		text-align: left;
		color: var(--m-fg);
	}
</style>
