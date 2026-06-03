<script lang="ts">
	import { onMount } from 'svelte';
	import { app } from './app.svelte';

	onMount(() => {
		void app.loadWorkspaces();
	});
</script>

<div class="screen">
	<div class="row" style="justify-content: space-between;">
		<h1>Workspaces</h1>
		<button class="ghost" onclick={() => app.loadWorkspaces()} disabled={app.loadingWorkspaces}>Refresh</button>
	</div>

	{#if app.loadingWorkspaces}
		<p class="muted">Loading…</p>
	{:else if app.workspaces.length === 0}
		<p class="muted">No workspaces found on this machine.</p>
	{:else}
		<div class="list">
			{#each app.workspaces as ws (ws.id)}
				<button class="card list-item" onclick={() => app.openWorkspace(ws.id)}>
					<div class="row">
						<span class="pip" class:live={ws.live}></span>
						<strong>{ws.name}</strong>
					</div>
					<span class="muted">{ws.live ? 'running' : 'stopped'} · {ws.id}</span>
				</button>
			{/each}
		</div>
	{/if}

	{#if app.error}
		<p class="error">{app.error}</p>
	{/if}

	<button class="ghost" onclick={() => app.unpair()}>Unpair this device</button>
</div>
