<script lang="ts">
	import { onMount } from 'svelte';
	import { app } from './app.svelte';
	import type { WorkspaceListing } from './app.svelte';

	onMount(() => {
		void app.loadWorkspaces();
	});

	/** Group workspaces by their owning IDE's id (Phase 14, ADR 0031).
	 * Local-carrier workspaces (empty `ide`) appear under "This
	 * machine"; remote-carrier workspaces appear under their IDE's
	 * label (the `ide` field is the id — we show it as-is until the
	 * bridge sends a human label too). */
	function groupByIde(wss: WorkspaceListing[]): [string, WorkspaceListing[]][] {
		const map = new Map<string, WorkspaceListing[]>();
		for (const ws of wss) {
			const key = ws.ide ?? '';
			const list = map.get(key);
			if (list) {
				list.push(ws);
			} else {
				map.set(key, [ws]);
			}
		}
		// Sort so the local group ("") is first.
		return [...map.entries()].toSorted(([a], [b]) => {
			if (a === '') {
				return -1;
			}
			if (b === '') {
				return 1;
			}
			return a.localeCompare(b);
		});
	}
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
		{@const groups = groupByIde(app.workspaces)}
		{#each groups as [ideLabel, wss]}
			{#if ideLabel}
				<h2 class="group-header">{ideLabel}</h2>
			{:else}
				<h2 class="group-header">This machine</h2>
			{/if}
			<div class="list">
				{#each wss as ws ((ws.ide ?? '') + '/' + ws.id)}
					<button class="card list-item" onclick={() => app.openWorkspace(ws.id, ws.ide ?? '', ws.name)}>
						<div class="row">
							<span class="pip" class:live={ws.live}></span>
							<strong>{ws.name}</strong>
						</div>
						<span class="muted">{ws.live ? 'running' : 'stopped'} · {ws.id}</span>
					</button>
				{/each}
			</div>
		{/each}
	{/if}

	<button class="ghost" onclick={() => app.unpair()}>Unpair this device</button>
</div>

<style>
	.group-header {
		font-size: 0.9rem;
		text-transform: uppercase;
		letter-spacing: 0.05em;
		color: var(--muted, #8b949e);
		margin-top: 1rem;
		margin-bottom: 0.3rem;
	}
	.group-header:first-child {
		margin-top: 0;
	}
</style>
