<script lang="ts">
	import { onMount } from 'svelte';
	import { app } from './lib/app.svelte';
	import PairScreen from './lib/PairScreen.svelte';
	import WorkspaceList from './lib/WorkspaceList.svelte';
	import WorkspaceView from './lib/WorkspaceView.svelte';
	import SessionView from './lib/SessionView.svelte';

	onMount(() => {
		void app.boot();
		// A backgrounded PWA's WebSocket drops; reconnect when the
		// user comes back to the app (specs/companion.md — "v1
		// reconnects on resume").
		const onVisible = (): void => {
			if (!document.hidden) {
				void app.ensureConnected();
			}
		};
		document.addEventListener('visibilitychange', onVisible);
		window.addEventListener('focus', onVisible);
		return () => {
			document.removeEventListener('visibilitychange', onVisible);
			window.removeEventListener('focus', onVisible);
		};
	});
</script>

{#if app.phase === 'connecting'}
	<div class="screen">
		<p class="muted">Connecting…</p>
	</div>
{:else if app.phase === 'pairing'}
	<PairScreen />
{:else if app.phase === 'error'}
	<div class="screen">
		<h2>Can't reach moon-ide</h2>
		<p class="error">{app.error}</p>
		<button class="ghost" onclick={() => app.unpair()}>Pair a different bridge</button>
		<button class="primary" onclick={() => app.boot()}>Retry</button>
	</div>
{:else if app.activeWorkspace && app.activeSession}
	<SessionView />
{:else if app.activeWorkspace}
	<WorkspaceView />
{:else}
	<WorkspaceList />
{/if}

{#if app.phase === 'ready' && app.error}
	<div class="toast" role="alert">
		<span class="toast-text">{app.error}</span>
		<button class="toast-dismiss" aria-label="Dismiss error" onclick={() => app.dismissError()}>×</button>
	</div>
{/if}

<style>
	.toast {
		position: fixed;
		left: 50%;
		transform: translateX(-50%);
		bottom: calc(env(safe-area-inset-bottom) + 4.5rem);
		max-width: min(92vw, 680px);
		display: flex;
		align-items: flex-start;
		gap: 0.5rem;
		background: var(--bg-elev-2);
		border: 1px solid var(--danger);
		border-radius: var(--radius);
		padding: 0.6rem 0.75rem;
		box-shadow: 0 4px 16px rgb(0 0 0 / 50%);
		z-index: 10;
	}
	.toast-text {
		color: var(--danger);
		font-size: 0.85rem;
		word-break: break-word;
	}
	.toast-dismiss {
		background: none;
		border: none;
		color: var(--fg-muted);
		font-size: 1.1rem;
		line-height: 1;
		padding: 0 0.2rem;
		min-height: 0;
	}
</style>
