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

	function confirmDelete(id: string, title: string): void {
		if (confirm(`Delete "${title || 'Untitled session'}"?`)) {
			void app.deleteSession(id);
		}
	}
</script>

<div class="screen">
	<div class="row head">
		<button class="ghost back" onclick={() => app.closeWorkspace()}>←</button>
		<strong class="workspace-name">{app.activeWorkspaceName}</strong>
		<button class="primary" onclick={() => app.newSession()}>+ New</button>
	</div>

	{#if app.folders.length > 1}
		<div class="projects" role="tablist" aria-label="Projects">
			{#each app.folders as f (f.path)}
				<button
					class="project-chip"
					class:active={f.path === app.activeFolder}
					role="tab"
					aria-selected={f.path === app.activeFolder}
					onclick={() => app.openFolder(f.path)}
				>
					{f.name}
				</button>
			{/each}
		</div>
	{/if}

	{#if app.coderStatus && !app.coderStatus.signed_in}
		<div class="card">
			<span class="muted">Coder is not signed in on the desktop — sign in there first.</span>
		</div>
	{/if}

	{#if app.loadingSessions}
		<p class="muted">Loading…</p>
	{:else if app.sessions.length === 0}
		<p class="muted">No coder sessions in this project yet.</p>
	{:else}
		<div class="list">
			{#each app.sessions as s (s.id)}
				<div class="card list-item session-row">
					<button class="list-item-main" onclick={() => app.openSession(s.id)}>
						<strong>
							{s.title || 'Untitled session'}
							{#if s.mode === 'coordinator'}<span
									class="badge"
									title="Coordinator — an orchestrator that spawns and manages worker agents">coord</span
								>{/if}
						</strong>
						<span class="muted">{relativeTime(s.updated_at_ms)}</span>
					</button>
					<button class="ghost danger" title="Delete session" onclick={() => confirmDelete(s.id, s.title)}>×</button>
				</div>
			{/each}
		</div>
	{/if}
</div>

<style>
	.head {
		gap: 0.5rem;
	}
	.back {
		flex: none;
		padding: 0.6rem 0.7rem;
	}
	.workspace-name {
		flex: 1;
		min-width: 0;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
		font-size: 1.05rem;
	}
	.projects {
		display: flex;
		gap: 0.4rem;
		overflow-x: auto;
		padding-bottom: 0.2rem;
		/* Chips scroll horizontally; don't let them wrap into a wall. */
		flex-wrap: nowrap;
		-webkit-overflow-scrolling: touch;
	}
	.project-chip {
		flex: none;
		min-height: 36px;
		padding: 0.3rem 0.8rem;
		border-radius: 999px;
		font-size: 0.85rem;
		color: var(--fg-muted);
		background: var(--bg-elev);
	}
	.project-chip.active {
		color: var(--accent-fg);
		background: var(--accent);
		border-color: var(--accent);
	}
	.session-row {
		/* The global `.list-item` stacks children vertically (for the
		   one-button workspace cards); a session row is a row — main
		   button + delete side by side. */
		flex-direction: row;
		align-items: center;
		gap: 0.3rem;
	}
	.list-item-main {
		flex: 1;
		min-width: 0;
		display: flex;
		flex-direction: column;
		gap: 0.2rem;
		background: none;
		border: none;
		cursor: pointer;
		text-align: left;
		color: inherit;
		padding: 0;
	}
	.list-item-main strong {
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.danger {
		flex: none;
		color: var(--danger);
		font-size: 1.1rem;
		padding: 0.2rem 0.5rem;
		border: none;
	}
</style>
