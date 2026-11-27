<script lang="ts">
	import { onMount } from 'svelte';
	import { MediaQuery } from 'svelte/reactivity';
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

	/** Same breakpoint as `#app` in styles.css. */
	const wide = new MediaQuery('min-width: 960px');

	// Mirror the current view into the URL hash so a refresh restores
	// it (see `app.routeHash` / `#restoreRoute`). `replaceState`
	// rather than assignment: no history entries, so the browser back
	// button can't trap the user in a loop of stale views. Gated on
	// `ready` so the pairing/error screens never clobber a route we
	// still want to restore after a retry.
	$effect(() => {
		const hash = app.routeHash;
		if (app.phase === 'ready' && location.hash !== hash) {
			history.replaceState(null, '', hash);
		}
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
{:else if app.activeWorkspace && wide.current}
	<div class="desk">
		<aside class="desk-side">
			<WorkspaceView />
		</aside>
		<main class="desk-main">
			{#if app.activeSession}
				{#key app.activeSession}
					<SessionView />
				{/key}
			{:else}
				<div class="desk-empty">
					<p class="muted">Open a session, or start one.</p>
					<button class="primary" onclick={() => app.newSession()}>New session</button>
				</div>
			{/if}
		</main>
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
	.desk {
		display: grid;
		grid-template-columns: var(--sidebar-width) minmax(0, 1fr);
		height: 100dvh;
	}
	.desk-side {
		overflow-y: auto;
		border-right: 1px solid var(--border);
		background: var(--bg-elev);
	}
	.desk-main {
		min-width: 0;
		display: flex;
		flex-direction: column;
	}
	/* The sidebar is the elevated surface; its cards sit back on the
	   page colour, as panels do in the IDE. */
	.desk-side :global(.card) {
		background: var(--bg);
	}
	/* Sessions are picked from the sidebar: no back button, and a
	   readable column centred in the wide pane. */
	.desk-main :global(.session) {
		padding: 0.75rem max(1rem, calc((100% - 920px) / 2));
	}
	.desk-main :global(.session-head .back) {
		display: none;
	}
	.desk-empty {
		margin: auto;
		display: flex;
		flex-direction: column;
		align-items: center;
		gap: 0.8rem;
	}
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
