<script lang="ts">
	import { onMount } from 'svelte';
	import { app } from './lib/app.svelte';
	import PairScreen from './lib/PairScreen.svelte';
	import WorkspaceList from './lib/WorkspaceList.svelte';
	import WorkspaceView from './lib/WorkspaceView.svelte';
	import SessionView from './lib/SessionView.svelte';

	onMount(() => {
		void app.boot();
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
