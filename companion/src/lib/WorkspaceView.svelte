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

	/** Provider picker disclosure (collapsed by default). */
	let providerOpen = $state(false);

	function pickProvider(id: string | null): void {
		providerOpen = false;
		void app.setProvider(id);
	}

	/** Commit composer state. */
	let commitMsg = $state('');
	let committing = $state(false);

	async function handleCommit(): Promise<void> {
		if (!app.scmStatus || app.scmStatus.changes.total === 0) {
			return;
		}
		committing = true;
		const result = await app.commit(commitMsg);
		committing = false;
		if (result) {
			commitMsg = '';
		}
	}

	async function suggestMsg(): Promise<void> {
		const msg = await app.suggestCommitMessage();
		if (msg) {
			commitMsg = msg;
		}
	}
</script>

<div class="screen">
	<div class="row head">
		<button class="ghost back" onclick={() => app.closeWorkspace()}>←</button>
		<strong class="workspace-name">{app.activeWorkspaceName}</strong>
		<button
			class="ghost coord-btn"
			title="New coordinator session — an orchestrator that spawns and manages worker agents"
			onclick={() => app.newCoordinatorSession()}>✦</button
		>
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
					{#if app.busyFolders.has(f.path)}
						<span class="pip live" title="An agent is running here"></span>
					{:else if app.folderAttention.has(f.path)}
						<span class="finished-dot" title="An agent finished here">✦</span>
					{/if}
				</button>
			{/each}
		</div>
	{/if}

	{#if app.coderStatus && !app.coderStatus.signed_in}
		<div class="card">
			<span class="muted">Coder is not signed in on the desktop — sign in there first.</span>
		</div>
	{/if}

	{#if app.modelSettings}
		{@const settings = app.modelSettings}
		{@const activeId = settings.active_provider ?? null}
		<div class="card provider-card">
			<button class="provider-row" onclick={() => (providerOpen = !providerOpen)} disabled={app.savingProvider}>
				<span class="muted">Provider</span>
				<strong class="provider-name">{app.providerLabel(activeId)}</strong>
				<span class="chevron">{providerOpen ? '▴' : '▾'}</span>
			</button>
			{#if providerOpen}
				<div class="provider-options">
					<button class="provider-option" class:selected={activeId === null} onclick={() => pickProvider(null)}>
						Hugging Face
					</button>
					{#each settings.providers as p (p.id)}
						<button class="provider-option" class:selected={activeId === p.id} onclick={() => pickProvider(p.id)}>
							{p.label || p.id}
						</button>
					{/each}
				</div>
			{/if}
			<label class="lock-row">
				<input
					type="checkbox"
					checked={settings.provider_lock != null}
					disabled={app.savingProvider}
					onchange={(e) => app.setProviderLock((e.target as HTMLInputElement).checked)}
				/>
				<span class="muted">
					Locked to this workspace
					{#if settings.provider_lock}
						— ignores the global default
					{/if}
				</span>
			</label>
		</div>
	{/if}

	{#if app.scmStatus}
		{@const scm = app.scmStatus}
		{@const defaultBranch = scm.branch.default_branch_remote_ref?.split('/').slice(1).join('/') ?? null}
		{@const onDefaultBranch = defaultBranch === null || scm.branch.name === defaultBranch}
		<div class="card scm-card">
			<div class="scm-head">
				<span class="scm-branch">{scm.branch.name || 'detached HEAD'}</span>
				{#if scm.branch.head_short_sha}
					<span class="muted scm-sha">{scm.branch.head_short_sha}</span>
				{/if}
				{#if scm.branch.ahead > 0}
					<span class="scm-ahead" title="Ahead of upstream">↑{scm.branch.ahead}</span>
				{/if}
				{#if scm.branch.behind > 0}
					<span class="scm-behind" title="Behind upstream">↓{scm.branch.behind}</span>
				{/if}
				{#if scm.branch.ahead > 0 || scm.branch.behind > 0}
					<button
						class="ghost scm-sync-btn"
						onclick={() => app.scmSync()}
						disabled={app.scmBusy}
						title={scm.branch.ahead > 0 && scm.branch.behind > 0
							? `Pull ${scm.branch.behind} and push ${scm.branch.ahead} (rebase first)`
							: scm.branch.ahead > 0
								? `Push ${scm.branch.ahead} commit${scm.branch.ahead === 1 ? '' : 's'}`
								: `Pull ${scm.branch.behind} commit${scm.branch.behind === 1 ? '' : 's'}`}
					>
						{app.scmBusy ? 'Syncing…' : 'Sync'}
					</button>
				{/if}
			</div>
			{#if !onDefaultBranch && defaultBranch}
				<button
					class="ghost scm-default-btn"
					onclick={() => app.scmSwitchBranch(defaultBranch)}
					disabled={app.scmBusy || scm.changes.total > 0}
					title={scm.changes.total > 0
						? 'Commit or discard the working-tree changes first'
						: `Switch the working tree back to ${defaultBranch}`}
				>
					⇄ Switch to {defaultBranch}
				</button>
			{/if}
			{#if scm.changes.total > 0}
				<div class="scm-changes">
					{#if scm.changes.added > 0}<span class="scm-change added">+{scm.changes.added}</span>{/if}
					{#if scm.changes.modified > 0}<span class="scm-change modified">~{scm.changes.modified}</span>{/if}
					{#if scm.changes.deleted > 0}<span class="scm-change deleted">-{scm.changes.deleted}</span>{/if}
					<span class="muted">{scm.changes.total} file{scm.changes.total !== 1 ? 's' : ''} changed</span>
				</div>
				<details class="scm-files">
					<summary>Show files</summary>
					<div class="scm-file-list">
						{#each scm.files as f (f.path)}
							<div class="scm-file">
								<span class="scm-file-status {f.status}">{f.status?.[0]?.toUpperCase()}</span>
								<span class="scm-file-path">{f.path}</span>
							</div>
						{/each}
					</div>
				</details>
				<div class="scm-commit">
					<textarea
						bind:value={commitMsg}
						placeholder="Commit message…"
						rows="2"
						disabled={committing || app.committing}
					></textarea>
					<div class="scm-commit-actions">
						<button class="ghost" onclick={suggestMsg} disabled={committing || app.committing} title="Suggest a message"
							>✦</button
						>
						<button class="primary" onclick={handleCommit} disabled={committing || app.committing || !commitMsg.trim()}
							>Commit</button
						>
					</div>
				</div>
			{:else}
				<span class="muted">No changes</span>
			{/if}
		</div>
	{:else if app.loadingScm}
		<div class="card"><span class="muted">Loading SCM…</span></div>
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
					{#if app.busySessions.has(s.id)}
						<span class="pip live" title="Running"></span>
					{:else}
						<span class="pip" title="Idle"></span>
					{/if}
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
	.project-chip .pip {
		margin-left: 0.3rem;
	}
	.finished-dot {
		margin-left: 0.3rem;
		color: var(--accent);
		font-size: 0.8rem;
	}
	.project-chip.active .finished-dot {
		color: var(--accent-fg);
	}
	.provider-card {
		display: flex;
		flex-direction: column;
		gap: 0.5rem;
		padding: 0.6rem 0.8rem;
	}
	.provider-row {
		display: flex;
		align-items: center;
		gap: 0.6rem;
		background: none;
		border: none;
		padding: 0;
		min-height: 32px;
		text-align: left;
		color: inherit;
	}
	.provider-name {
		flex: 1;
		min-width: 0;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.chevron {
		color: var(--fg-muted);
	}
	.provider-options {
		display: flex;
		flex-direction: column;
		gap: 0.25rem;
	}
	.provider-option {
		background: none;
		border: 1px solid var(--border);
		border-radius: var(--radius);
		color: var(--fg);
		text-align: left;
		padding: 0.4rem 0.6rem;
		min-height: 40px;
		font-size: 0.9rem;
	}
	.provider-option.selected {
		border-color: var(--accent);
		background: var(--bg-elev-2);
	}
	.lock-row {
		display: flex;
		align-items: center;
		gap: 0.5rem;
		font-size: 0.85rem;
		min-height: 28px;
	}
	.lock-row input {
		width: auto;
		min-height: 0;
		accent-color: var(--accent);
	}
	.scm-card {
		display: flex;
		flex-direction: column;
		gap: 0.4rem;
		padding: 0.6rem 0.8rem;
	}
	.scm-head {
		display: flex;
		align-items: center;
		gap: 0.4rem;
	}
	.scm-branch {
		font-weight: 600;
		font-size: 0.9rem;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.scm-sha {
		font-family: var(--mono, monospace);
		font-size: 0.75rem;
	}
	.scm-sync-btn {
		margin-left: auto;
		padding: 0.15rem 0.6rem;
		font-size: 0.75rem;
		line-height: 1.3;
	}
	.scm-default-btn {
		align-self: flex-start;
		padding: 0.2rem 0.6rem;
		font-size: 0.75rem;
		border: 1px solid var(--border);
		border-radius: 999px;
	}
	.scm-ahead {
		font-size: 0.75rem;
		color: var(--accent);
	}
	.scm-behind {
		font-size: 0.75rem;
		color: var(--fg-muted);
	}
	.scm-changes {
		display: flex;
		align-items: center;
		gap: 0.5rem;
		font-size: 0.85rem;
	}
	.scm-change {
		font-weight: 600;
		font-size: 0.8rem;
	}
	.scm-change.added {
		color: #3fb950;
	}
	.scm-change.modified {
		color: #d29922;
	}
	.scm-change.deleted {
		color: #f85149;
	}
	.scm-files summary {
		cursor: pointer;
		font-size: 0.8rem;
		color: var(--fg-muted);
	}
	.scm-file-list {
		display: flex;
		flex-direction: column;
		gap: 0.15rem;
		margin-top: 0.3rem;
		max-height: 200px;
		overflow-y: auto;
	}
	.scm-file {
		display: flex;
		align-items: center;
		gap: 0.4rem;
		font-size: 0.8rem;
	}
	.scm-file-status {
		flex: none;
		width: 1.2rem;
		text-align: center;
		font-weight: 700;
		font-size: 0.7rem;
	}
	.scm-file-status.added {
		color: #3fb950;
	}
	.scm-file-status.modified {
		color: #d29922;
	}
	.scm-file-status.deleted {
		color: #f85149;
	}
	.scm-file-status.untracked {
		color: #3fb950;
	}
	.scm-file-status.conflicted {
		color: #f85149;
	}
	.scm-file-path {
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
		font-family: var(--mono, monospace);
		font-size: 0.75rem;
	}
	.scm-commit {
		display: flex;
		flex-direction: column;
		gap: 0.4rem;
	}
	.scm-commit textarea {
		resize: none;
		font: inherit;
		background: var(--bg-elev);
		color: var(--fg);
		border: 1px solid var(--border);
		border-radius: var(--radius);
		padding: 0.4rem 0.5rem;
	}
	.scm-commit-actions {
		display: flex;
		gap: 0.4rem;
	}
	.scm-commit-actions .primary {
		flex: 1;
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
	.coord-btn {
		flex: none;
		font-size: 1.1rem;
		padding: 0.4rem 0.5rem;
		line-height: 1;
	}
	.badge {
		font-size: 0.7rem;
		font-weight: 600;
		padding: 0.1em 0.4em;
		border-radius: 999px;
		background: var(--accent);
		color: var(--accent-fg, #fff);
		margin-left: 0.3rem;
		vertical-align: middle;
		text-transform: uppercase;
		letter-spacing: 0.03em;
	}
</style>
