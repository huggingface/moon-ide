<script lang="ts">
	type Props = {
		onPickFolder: () => void | Promise<void>;
	};
	let { onPickFolder }: Props = $props();
</script>

<div class="welcome">
	<div class="card">
		<h1>moon-ide</h1>
		<p class="subtitle">Open a folder to begin.</p>
		<button class="primary" onclick={() => void onPickFolder()}>Open folder</button>
		<p class="hint">Phase 0 skeleton. See <code>specs/roadmap.md</code> for what's next.</p>
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
		max-width: 420px;
	}
	h1 {
		margin: 0 0 8px;
		font-size: 28px;
		font-weight: 600;
		letter-spacing: -0.01em;
	}
	.subtitle {
		margin: 0 0 24px;
		color: var(--m-fg-muted);
	}
	.primary {
		background: var(--m-accent);
		color: #0d1017;
		font-weight: 600;
		border-radius: 6px;
		padding: 8px 18px;
	}
	.primary:hover {
		background: var(--m-accent-strong);
	}
	.hint {
		margin-top: 28px;
		font-size: 12px;
		color: var(--m-fg-subtle);
	}
	code {
		background: var(--m-bg-2);
		padding: 1px 4px;
		border-radius: 3px;
		font-family: var(--m-font-mono);
	}
</style>
