<script lang="ts">
	import { app } from './app.svelte';

	function relativeTime(ms: number): string {
		const diff = Date.now() - ms;
		const mins = Math.round(diff / 60000);
		if (mins < 1) {
			return 'just now';
		}
		if (mins < 60) {
			return `${mins}m ago`;
		}
		const hours = Math.round(mins / 60);
		if (hours < 24) {
			return `${hours}h ago`;
		}
		return `${Math.round(hours / 24)}d ago`;
	}
</script>

<div class="screen">
	<div class="row" style="justify-content: space-between;">
		<button class="ghost" onclick={() => app.closeWorkspace()}>← Workspaces</button>
		<strong>{app.activeWorkspace}</strong>
	</div>

	{#if app.coderStatus}
		<div class="card row" style="justify-content: space-between;">
			<span>
				{#if app.coderStatus.signed_in}
					Coder signed in
				{:else}
					<span class="muted">Coder not signed in</span>
				{/if}
			</span>
			{#if app.coderStatus.running_turn}
				<span class="muted">running…</span>
			{/if}
		</div>
	{/if}

	<h2>Sessions</h2>
	{#if app.loadingSessions}
		<p class="muted">Loading…</p>
	{:else if app.sessions.length === 0}
		<p class="muted">No coder sessions in this workspace's active folder.</p>
	{:else}
		<div class="list">
			{#each app.sessions as s (s.id)}
				<button class="card list-item" onclick={() => app.openSession(s.id)}>
					<strong>
						{s.title || 'Untitled session'}
						{#if s.mode === 'coordinator'}<span
								class="badge"
								title="Coordinator — an orchestrator that spawns and manages worker agents">coord</span
							>{/if}
					</strong>
					<span class="muted">{relativeTime(s.updated_at_ms)}</span>
				</button>
			{/each}
		</div>
	{/if}

	{#if app.error}
		<p class="error">{app.error}</p>
	{/if}
</div>
