<script lang="ts">
	import { diffSections } from './diff';
	// Project management mode: chips flip from "switch" to "remove"
	// targets, with a type-the-name confirm dialog (removal is
	// mirrored on the desktop, so friction is deliberate).
	let manageProjects = $state(false);
	let removeTarget = $state<{ path: string; name: string } | null>(null);
	let removeConfirm = $state('');

	function openRemoveDialog(path: string, name: string): void {
		removeTarget = { path, name };
		removeConfirm = '';
	}

	async function confirmRemove(): Promise<void> {
		const target = removeTarget;
		if (!target || removeConfirm !== target.name) {
			return;
		}
		removeTarget = null;
		manageProjects = false;
		await app.removeFolder(target.path);
	}
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
	let servicesOpen = $state(false);

	/** Standard-model editor (HF route only): tap the model row to
	 * edit the slug inline; Enter/Save persists via the same
	 * settings payload the desktop picker writes. */
	let editingModel = $state(false);
	let modelDraft = $state('');

	function startModelEdit(current: string): void {
		modelDraft = current;
		editingModel = true;
	}

	let editingRotation = $state(false);
	let rotationAddDraft = $state('');

	/** Move the fallback at `i` by `delta` and persist. */
	function moveFallback(current: string[], i: number, delta: number): void {
		const j = i + delta;
		if (j < 0 || j >= current.length) {
			return;
		}
		const next = [...current];
		[next[i], next[j]] = [next[j] as string, next[i] as string];
		void app.setRotation(next);
	}

	function removeFallback(current: string[], i: number): void {
		void app.setRotation(current.toSpliced(i, 1));
	}

	function addFallback(current: string[]): void {
		const slug = rotationAddDraft.trim();
		if (!slug) {
			return;
		}
		rotationAddDraft = '';
		void app.setRotation([...current, slug]);
	}

	function saveModel(): void {
		editingModel = false;
		void app.setStandardModel(modelDraft);
	}

	function onModelKeydown(e: KeyboardEvent): void {
		if (e.key === 'Enter') {
			e.preventDefault();
			saveModel();
		} else if (e.key === 'Escape') {
			e.preventDefault();
			editingModel = false;
		}
	}

	function pickProvider(id: string | null): void {
		providerOpen = false;
		void app.setProvider(id);
	}

	// Add-provider form. Kind presets pre-fill the endpoint; the
	// base URL stays editable only for custom entries.
	let addingProvider = $state(false);
	let apKind = $state<'open_router' | 'anthropic' | 'custom'>('open_router');
	let apLabel = $state('OpenRouter');
	let apBaseUrl = $state('https://openrouter.ai/api/v1');
	let apKey = $state('');
	let apModel = $state('');
	let apCheapModel = $state('');
	let apPayloadCap = $state('');

	const AP_PRESETS = {
		open_router: { label: 'OpenRouter', baseUrl: 'https://openrouter.ai/api/v1' },
		anthropic: { label: 'Anthropic', baseUrl: 'https://api.anthropic.com' },
		custom: { label: '', baseUrl: '' },
	} as const;

	function pickApKind(kind: 'open_router' | 'anthropic' | 'custom'): void {
		apKind = kind;
		apLabel = AP_PRESETS[kind].label;
		apBaseUrl = AP_PRESETS[kind].baseUrl;
	}

	const apReady = $derived(
		apLabel.trim() !== '' && apBaseUrl.trim() !== '' && apKey.trim() !== '' && apModel.trim() !== '',
	);

	async function submitProvider(): Promise<void> {
		if (!apReady || app.savingProvider) {
			return;
		}
		const capMb = parseInt(apPayloadCap, 10);
		const ok = await app.addProvider({
			kind: apKind,
			label: apLabel.trim(),
			baseUrl: apBaseUrl.trim(),
			apiKey: apKey.trim(),
			standardModel: apModel.trim(),
			cheapModel: apCheapModel.trim(),
			payloadCapMb: Number.isFinite(capMb) && capMb > 0 ? capMb : null,
		});
		if (ok) {
			addingProvider = false;
			providerOpen = false;
			apKey = '';
			apModel = '';
			apCheapModel = '';
			apPayloadCap = '';
		}
	}

	/** Which file the changes overlay should auto-expand (null =
	 * all collapsed; set when the user tapped a specific file row). */
	let changesFocusFile = $state<string | null>(null);

	async function openChanges(file: string | null): Promise<void> {
		changesFocusFile = file;
		await app.loadWorkingDiff();
	}

	/** Commit composer state. */
	let commitMsg = $state('');
	let committing = $state(false);

	async function handleCommit(): Promise<void> {
		if (!app.scmStatus || (app.scmStatus.changes?.total ?? 0) === 0) {
			return;
		}
		committing = true;
		const result = await app.commit(commitMsg);
		committing = false;
		if (result) {
			commitMsg = '';
		}
	}

	/** True while the fast model is drafting a commit subject. */
	let suggesting = $state(false);

	async function suggestMsg(): Promise<void> {
		suggesting = true;
		try {
			const msg = await app.suggestCommitMessage();
			if (msg) {
				commitMsg = msg;
			}
		} finally {
			suggesting = false;
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
					class:removing={manageProjects}
					role="tab"
					aria-selected={f.path === app.activeFolder}
					onclick={() => (manageProjects ? openRemoveDialog(f.path, f.name) : app.openFolder(f.path))}
				>
					{#if manageProjects}<span class="chip-x">✕</span>{/if}
					{f.name}
					{#if app.busyFolders.has(f.path)}
						<span class="pip live" title="An agent is running here"></span>
					{:else if app.folderAttention.has(f.path)}
						<span class="finished-dot" title="An agent finished here">✦</span>
					{/if}
				</button>
			{/each}
			<button
				class="ghost manage-projects"
				title={manageProjects ? 'Done managing projects' : 'Remove a project from this workspace'}
				onclick={() => (manageProjects = !manageProjects)}>{manageProjects ? 'done' : '✎'}</button
			>
		</div>
	{/if}

	{#if app.workingDiff !== null}
		{@const sections = diffSections(app.workingDiff)}
		{@const changedFiles = app.scmStatus?.files ?? []}
		<div class="review-overlay">
			<div class="row review-head">
				<button class="ghost back" onclick={() => app.closeWorkingDiff()}>←</button>
				<strong class="review-title">Uncommitted changes</strong>
				<button
					class="ghost scm-refresh"
					title="Refresh"
					disabled={app.loadingWorkingDiff}
					onclick={() => {
						void app.loadScmStatus();
						void app.loadWorkingDiff();
					}}>⟳</button
				>
			</div>
			<div class="review-body">
				{#if changedFiles.length === 0}
					<p class="muted">Working tree is clean.</p>
				{:else}
					<p class="muted review-summary">
						{changedFiles.length} file{changedFiles.length === 1 ? '' : 's'} changed vs HEAD (untracked included; diff truncated
						at 64 kB)
					</p>
					{#each changedFiles as f (f.path)}
						<details class="review-file" open={f.path === changesFocusFile}>
							<summary>
								<span class="review-file-status {f.status}">{f.status?.[0]?.toUpperCase()}</span>
								<span class="review-file-path">{f.path}</span>
							</summary>
							{#if sections.has(f.path)}
								<pre class="diff-body">{sections.get(f.path)}</pre>
							{:else}
								<p class="muted review-nodiff">No text diff (binary, rename, or diff truncated).</p>
							{/if}
						</details>
					{/each}
				{/if}
			</div>
		</div>
	{/if}

	{#if removeTarget}
		<div class="remove-overlay" role="dialog" aria-modal="true" aria-label="Remove project">
			<div class="card remove-card">
				<h3>Remove project</h3>
				<p>
					This unbinds <strong>{removeTarget.name}</strong> from the workspace
					<strong>everywhere — the desktop's folder bar loses it too</strong> (same shared workspace state). Files on disk
					are untouched; re-open the folder to bring it back.
				</p>
				<p>Type <code>{removeTarget.name}</code> to confirm:</p>
				<!-- svelte-ignore a11y_autofocus -->
				<input
					class="remove-input"
					autofocus
					autocomplete="off"
					autocapitalize="off"
					spellcheck="false"
					bind:value={removeConfirm}
					placeholder={removeTarget.name}
				/>
				<div class="remove-actions">
					<button class="ghost" onclick={() => (removeTarget = null)}>Cancel</button>
					<button class="remove-btn" disabled={removeConfirm !== removeTarget.name} onclick={() => void confirmRemove()}
						>Remove</button
					>
				</div>
			</div>
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
					<button class="provider-option add-provider" onclick={() => (addingProvider = !addingProvider)}>
						{addingProvider ? '− Cancel' : '+ Add provider…'}
					</button>
				</div>
				{#if addingProvider}
					<div class="ap-form">
						<div class="ap-kinds">
							{#each [['open_router', 'OpenRouter'], ['anthropic', 'Anthropic'], ['custom', 'Custom']] as [kind, label] (kind)}
								<button
									class="ap-kind"
									class:selected={apKind === kind}
									onclick={() => pickApKind(kind as 'open_router' | 'anthropic' | 'custom')}>{label}</button
								>
							{/each}
						</div>
						{#if apKind === 'custom'}
							<input class="ap-input" bind:value={apLabel} placeholder="Label" spellcheck="false" />
							<input
								class="ap-input"
								bind:value={apBaseUrl}
								placeholder="Base URL (OpenAI-compat /v1 root)"
								spellcheck="false"
								autocapitalize="off"
							/>
						{/if}
						<input
							class="ap-input"
							bind:value={apKey}
							placeholder="API key"
							type="password"
							autocomplete="off"
							spellcheck="false"
						/>
						<input
							class="ap-input"
							bind:value={apModel}
							placeholder={apKind === 'anthropic'
								? 'Model (e.g. claude-sonnet-4-5)'
								: 'Model (e.g. anthropic/claude-sonnet-4.5)'}
							spellcheck="false"
							autocapitalize="off"
						/>
						<input
							class="ap-input"
							bind:value={apCheapModel}
							placeholder="Cheap model (optional — titles, summaries)"
							spellcheck="false"
							autocapitalize="off"
						/>
						<input
							class="ap-input"
							bind:value={apPayloadCap}
							placeholder="Payload cap MB (optional — elides old screenshots)"
							inputmode="numeric"
							spellcheck="false"
						/>
						<button
							class="primary ap-submit"
							disabled={!apReady || app.savingProvider}
							onclick={() => void submitProvider()}
						>
							{app.savingProvider ? 'Checking…' : 'Probe & add'}
						</button>
					</div>
				{/if}
			{/if}
			{#if activeId === null}
				{@const modelSlug = settings.resolved_standard_model ?? ''}
				{#if editingModel}
					<div class="model-row">
						<span class="muted">Model</span>
						<!-- svelte-ignore a11y_autofocus -->
						<input
							class="model-input"
							autofocus
							bind:value={modelDraft}
							onkeydown={onModelKeydown}
							placeholder="owner/name[:provider] — empty = default"
							spellcheck="false"
							disabled={app.savingProvider}
						/>
						<button class="ghost" onclick={saveModel} disabled={app.savingProvider}>Save</button>
					</div>
				{:else}
					<button
						class="model-row model-row-btn"
						title="Tap to change the model"
						onclick={() => startModelEdit(modelSlug)}
					>
						<span class="muted">Model</span>
						<span class="model-slug">{modelSlug || '(default)'}</span>
					</button>
				{/if}
				{@const rotation = settings.rotation ?? []}
				{#if editingRotation}
					<div class="rotation-editor">
						<div class="model-row">
							<span class="muted">Fallbacks</span>
							<button class="ghost" onclick={() => (editingRotation = false)}>Done</button>
						</div>
						{#each rotation as slug, i (slug + i)}
							<div class="rotation-row">
								<span class="model-slug">{slug}</span>
								<button
									class="ghost rot-btn"
									title="Try earlier"
									disabled={app.savingProvider || i === 0}
									onclick={() => moveFallback(rotation, i, -1)}>↑</button
								>
								<button
									class="ghost rot-btn"
									title="Try later"
									disabled={app.savingProvider || i === rotation.length - 1}
									onclick={() => moveFallback(rotation, i, 1)}>↓</button
								>
								<button
									class="ghost rot-btn danger"
									title="Remove from the chain"
									disabled={app.savingProvider}
									onclick={() => removeFallback(rotation, i)}>✕</button
								>
							</div>
						{/each}
						<div class="rotation-row">
							<input
								class="model-input"
								bind:value={rotationAddDraft}
								onkeydown={(e) => {
									if (e.key === 'Enter') {
										addFallback(rotation);
									} else if (e.key === 'Escape') {
										editingRotation = false;
									}
								}}
								placeholder="owner/name[:provider] to append"
								spellcheck="false"
								disabled={app.savingProvider}
							/>
							<button
								class="ghost"
								onclick={() => addFallback(rotation)}
								disabled={app.savingProvider || !rotationAddDraft.trim()}>Add</button
							>
						</div>
					</div>
				{:else}
					<button
						class="model-row model-row-btn"
						title="Models tried in order when the active one stays rate-limited through backoff"
						onclick={() => (editingRotation = true)}
					>
						<span class="muted">Fallbacks</span>
						<span class="model-slug">{rotation.length > 0 ? rotation.join(' → ') : '(none)'}</span>
					</button>
				{/if}
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

	{#if app.containerSnapshot || Object.keys(app.projectCompose).length > 0}
		{@const snap = app.containerSnapshot}
		{@const shellState = snap?.status.state ?? null}
		{@const action = snap?.action ?? null}
		{@const composeFolders = Object.values(app.projectCompose).filter((pc) => pc.compose_file)}
		{@const actionLabel = (a: { action: string; folder?: string; service?: string }): string => {
			const short = a.action.replace('project_compose_', '').replace('project_compose', '').replace(/_/g, ' ');
			const target = a.service ?? a.folder?.split('/').at(-1) ?? '';
			return `${short}${target ? ` — ${target}` : ''}…`;
		}}
		<div class="card services-card">
			<button class="provider-row" onclick={() => (servicesOpen = !servicesOpen)}>
				<span class="muted">Services</span>
				<strong class="provider-name">
					{#if shellState === 'running'}dev running{:else if shellState === 'paused'}dev paused{:else if shellState === 'absent'}dev
						off{:else}{shellState ?? 'unknown'}{/if}
					{#if composeFolders.length > 0}
						· {composeFolders.length} project{composeFolders.length === 1 ? '' : 's'}
					{/if}
				</strong>
				<span class="chevron">{servicesOpen ? '▴' : '▾'}</span>
			</button>
			{#if servicesOpen}
				<div class="services-body">
					{#if action}
						<p class="services-action {action.error ? 'error' : ''}">
							{#if !action.finished}
								{actionLabel(action)}
							{:else if action.error}
								{action.error}
							{:else}
								{actionLabel(action).replace('…', '')} done
							{/if}
						</p>
					{/if}
					{#if shellState}
						<div class="services-row">
							<span class="muted">Dev container</span>
							<span class="services-btns">
								{#if shellState === 'absent'}
									<button
										class="ghost"
										disabled={app.containerBusy}
										onclick={() => app.containerAction('container_setup')}>Start</button
									>
								{:else if shellState === 'running'}
									<button
										class="ghost"
										disabled={app.containerBusy}
										onclick={() => app.containerAction('container_pause')}>Pause</button
									>
									<button
										class="ghost"
										disabled={app.containerBusy}
										onclick={() => app.containerAction('container_stop')}>Stop</button
									>
								{:else if shellState === 'paused'}
									<button
										class="ghost"
										disabled={app.containerBusy}
										onclick={() => app.containerAction('container_resume')}>Resume</button
									>
								{:else}
									<button
										class="ghost"
										disabled={app.containerBusy}
										onclick={() => app.containerAction('container_setup')}>Recreate</button
									>
								{/if}
								{#if shellState !== 'absent'}
									<button
										class="ghost"
										disabled={app.containerBusy}
										title="Stop and remove the compose project (compose.yaml stays)"
										onclick={() => app.containerAction('container_teardown')}>Teardown</button
									>
								{/if}
							</span>
						</div>
					{/if}
					{#each composeFolders as pc (pc.folder_path)}
						<div class="services-folder">
							<div class="services-row">
								<strong class="services-folder-name">{pc.folder_path.split('/').at(-1)}</strong>
								<span class="services-btns">
									{#if pc.status.state === 'running'}
										<button
											class="ghost"
											disabled={app.containerBusy}
											onclick={() => app.projectComposeAction('project_compose_pause', pc.folder_path)}>Pause</button
										>
										<button
											class="ghost"
											disabled={app.containerBusy}
											onclick={() => app.projectComposeAction('project_compose_stop', pc.folder_path)}>Stop</button
										>
									{:else}
										<button
											class="ghost"
											disabled={app.containerBusy}
											onclick={() => app.projectComposeAction('project_compose_up', pc.folder_path)}
											>{pc.status.state === 'absent' ? 'Start' : 'Resume'}</button
										>
										{#if pc.status.state !== 'absent'}
											<button
												class="ghost"
												disabled={app.containerBusy}
												onclick={() => app.projectComposeAction('project_compose_down', pc.folder_path)}>Down</button
											>
										{/if}
									{/if}
								</span>
							</div>
							{#each pc.status.services as svc (svc.name)}
								<div class="services-svc {svc.raw_state}">
									<span class="svc-name">{svc.name}</span>
									<span class="svc-state"
										>{svc.raw_state}{svc.health ? ` · ${svc.health}` : ''}{svc.raw_state === 'exited' &&
										svc.exit_code !== 0
											? ` (${svc.exit_code})`
											: ''}</span
									>
									{#if svc.raw_state === 'running'}
										<button
											class="ghost svc-btn"
											disabled={app.containerBusy}
											onclick={() =>
												app.projectComposeAction('project_compose_service_restart', pc.folder_path, svc.name)}>↻</button
										>
									{:else if svc.raw_state === 'exited' || svc.raw_state === 'created'}
										<button
											class="ghost svc-btn"
											disabled={app.containerBusy}
											onclick={() =>
												app.projectComposeAction('project_compose_service_start', pc.folder_path, svc.name)}>▶</button
										>
									{/if}
								</div>
							{/each}
						</div>
					{/each}
				</div>
			{/if}
		</div>
	{/if}

	{#if app.scmStatus}
		{@const scm = app.scmStatus}
		{@const branch = scm.branch}
		<!-- Wire-tolerant defaults (see ScmStatus): a partial payload
		     from a mismatched IDE build renders as "No changes". -->
		{@const changes = scm.changes ?? { added: 0, modified: 0, deleted: 0, total: 0 }}
		{@const files = scm.files ?? []}
		{@const defaultBranch = branch?.default_branch_remote_ref?.split('/').slice(1).join('/') ?? null}
		{@const onDefaultBranch = !branch || defaultBranch === null || branch.name === defaultBranch}
		<div class="card scm-card">
			{#if branch}
				<div class="scm-head">
					<span class="scm-branch">{branch.name || 'detached HEAD'}</span>
					<button
						class="ghost scm-refresh"
						title="Refresh git status"
						disabled={app.loadingScm}
						onclick={() => void app.loadScmStatus()}>{app.loadingScm ? '…' : '⟳'}</button
					>
					{#if branch.head_short_sha}
						<span class="muted scm-sha">{branch.head_short_sha}</span>
					{/if}
					{#if branch.ahead > 0}
						<span class="scm-ahead" title="Ahead of upstream">↑{branch.ahead}</span>
					{/if}
					{#if branch.behind > 0}
						<span class="scm-behind" title="Behind upstream">↓{branch.behind}</span>
					{/if}
					{#if branch.ahead > 0 || branch.behind > 0}
						<button
							class="ghost scm-sync-btn"
							onclick={() => app.scmSync()}
							disabled={app.scmBusy}
							title={branch.ahead > 0 && branch.behind > 0
								? `Pull ${branch.behind} and push ${branch.ahead} (rebase first)`
								: branch.ahead > 0
									? `Push ${branch.ahead} commit${branch.ahead === 1 ? '' : 's'}`
									: `Pull ${branch.behind} commit${branch.behind === 1 ? '' : 's'}`}
						>
							{app.scmBusy ? 'Syncing…' : 'Sync'}
						</button>
					{/if}
				</div>
				{#if !onDefaultBranch && defaultBranch}
					<button
						class="ghost scm-default-btn"
						onclick={() => app.scmSwitchBranch(defaultBranch)}
						disabled={app.scmBusy || changes.total > 0}
						title={changes.total > 0
							? 'Commit or discard the working-tree changes first'
							: `Switch the working tree back to ${defaultBranch}`}
					>
						⇄ Switch to {defaultBranch}
					</button>
				{/if}
				{#if onDefaultBranch && branch.previous_branch && branch.previous_branch !== branch.name}
					<button
						class="ghost scm-default-btn"
						onclick={() => app.scmSwitchBranch(branch.previous_branch!)}
						disabled={app.scmBusy || changes.total > 0}
						title={changes.total > 0
							? 'Commit or discard the working-tree changes first'
							: `Switch back to ${branch.previous_branch}`}
					>
						⇄ Switch to {branch.previous_branch}
					</button>
				{/if}
			{/if}
			{#if changes.total > 0}
				<div class="scm-changes">
					{#if changes.added > 0}<span class="scm-change added">+{changes.added}</span>{/if}
					{#if changes.modified > 0}<span class="scm-change modified">~{changes.modified}</span>{/if}
					{#if changes.deleted > 0}<span class="scm-change deleted">-{changes.deleted}</span>{/if}
					<span class="muted">{changes.total} file{changes.total !== 1 ? 's' : ''} changed</span>
				</div>
				<details class="scm-files">
					<summary>Show files</summary>
					<div class="scm-file-list">
						{#each files as f (f.path)}
							<button class="scm-file" onclick={() => void openChanges(f.path)}>
								<span class="scm-file-status {f.status}">{f.status?.[0]?.toUpperCase()}</span>
								<span class="scm-file-path">{f.path}</span>
							</button>
						{/each}
					</div>
				</details>
				<button class="ghost scm-view-changes" disabled={app.loadingWorkingDiff} onclick={() => void openChanges(null)}
					>{app.loadingWorkingDiff ? 'Loading diff…' : 'View changes'}</button
				>
				<div class="scm-commit">
					<textarea
						bind:value={commitMsg}
						placeholder="Commit message…"
						rows="2"
						disabled={committing || app.committing}
					></textarea>
					<div class="scm-commit-actions">
						<button
							class="ghost"
							class:suggesting
							onclick={suggestMsg}
							disabled={suggesting || committing || app.committing}
							title="Suggest a message"
						>
							✦
						</button>
						<button
							class="primary"
							onclick={handleCommit}
							disabled={suggesting || committing || app.committing || !commitMsg.trim()}
						>
							Commit
						</button>
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
					<button
						class="list-item-main"
						onclick={(e) => {
							// Ctrl/Cmd-click (and middle-click via auxclick
							// below): open the session in a fresh browser
							// tab through its hash route, desktop-browser
							// style, leaving this tab where it is.
							if (e.ctrlKey || e.metaKey) {
								window.open(app.sessionRouteHash(s.id), '_blank');
								return;
							}
							app.openSession(s.id);
						}}
						onauxclick={(e) => {
							if (e.button === 1) {
								window.open(app.sessionRouteHash(s.id), '_blank');
							}
						}}
					>
						<span class="title-line">
							{#if s.mode === 'coordinator'}<span
									class="badge"
									title="Coordinator — an orchestrator that spawns and manages worker agents">coord</span
								>{/if}
							<strong>{s.title || 'Untitled session'}</strong>
						</span>
						<span class="muted">{relativeTime(s.updated_at_ms)}</span>
					</button>
					{#if app.busySessions.has(s.id)}
						<span class="pip live" title="Running"></span>
					{:else if s.last_error}
						<span class="pip failed" title="Last turn failed — open to retry">!</span>
					{:else if s.interrupted}
						<span class="pip interrupted" title="Turn never finished (restart/stop) — open to relaunch">!</span>
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
	.services-card {
		display: flex;
		flex-direction: column;
		gap: 0.5rem;
		padding: 0.6rem 0.8rem;
	}
	.services-body {
		display: flex;
		flex-direction: column;
		gap: 0.45rem;
	}
	.services-action {
		margin: 0;
		font-size: 0.78rem;
		color: var(--fg-muted);
	}
	.services-action.error {
		color: var(--danger, #e5484d);
		overflow-wrap: anywhere;
	}
	.services-row {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 0.5rem;
	}
	.services-btns {
		display: flex;
		gap: 0.3rem;
		flex-wrap: wrap;
		justify-content: flex-end;
	}
	.services-folder {
		display: flex;
		flex-direction: column;
		gap: 0.2rem;
		border-top: 1px solid var(--border, rgba(127, 127, 127, 0.25));
		padding-top: 0.4rem;
	}
	.services-folder-name {
		font-size: 0.85rem;
	}
	.services-svc {
		display: flex;
		align-items: center;
		gap: 0.5rem;
		font-size: 0.75rem;
		padding-left: 0.4rem;
	}
	.services-svc .svc-name {
		min-width: 5.5rem;
		font-weight: 600;
	}
	.services-svc .svc-state {
		color: var(--fg-muted);
		flex: 1;
	}
	.services-svc.exited .svc-state {
		color: var(--danger, #e5484d);
	}
	.svc-btn {
		padding: 0.05rem 0.4rem;
		font-size: 0.75rem;
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
	.model-row {
		display: flex;
		align-items: center;
		gap: 0.5rem;
		font-size: 0.85rem;
		min-height: 28px;
		width: 100%;
	}
	.model-row-btn {
		background: none;
		border: none;
		padding: 0;
		color: inherit;
		text-align: left;
		cursor: pointer;
	}
	.model-slug {
		font-family: var(--mono, monospace);
		font-size: 0.8rem;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.rotation-editor {
		display: flex;
		flex-direction: column;
		gap: 0.35rem;
	}
	.rotation-row {
		display: flex;
		align-items: center;
		gap: 0.35rem;
	}
	.rotation-row .model-slug {
		flex: 1;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.rot-btn {
		padding: 0.15rem 0.5rem;
	}
	.rot-btn.danger {
		color: var(--danger, #e5484d);
	}
	.model-input {
		flex: 1;
		min-width: 0;
		font-family: var(--mono, monospace);
		font-size: 0.8rem;
		background: var(--bg-elev-2);
		color: var(--fg);
		border: 1px solid var(--accent);
		border-radius: var(--radius);
		padding: 0.3rem 0.5rem;
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
	.add-provider {
		color: var(--fg-muted);
	}
	.ap-form {
		display: flex;
		flex-direction: column;
		gap: 0.45rem;
		padding: 0.5rem 0 0.2rem;
	}
	.ap-kinds {
		display: flex;
		gap: 0.4rem;
	}
	.ap-kind {
		flex: 1;
		padding: 0.3rem 0;
		border: 1px solid var(--border);
		border-radius: 6px;
		background: none;
		color: var(--fg-muted);
		font-size: 0.8rem;
	}
	.ap-kind.selected {
		border-color: var(--accent, #7aa2f7);
		color: var(--fg);
	}
	.ap-input {
		width: 100%;
		box-sizing: border-box;
		font-size: 0.85rem;
	}
	.ap-submit {
		align-self: flex-end;
		padding: 0.35rem 0.9rem;
	}
	.scm-refresh {
		flex-shrink: 0;
		font-size: 0.85rem;
		padding: 0.15rem 0.5rem;
		min-height: 0;
	}
	.scm-view-changes {
		font-size: 0.78rem;
		padding: 0.25rem 0.6rem;
		margin-top: 0.3rem;
	}
	.scm-file {
		display: flex;
		gap: 0.4rem;
		align-items: baseline;
		background: none;
		border: none;
		padding: 0.15rem 0;
		width: 100%;
		text-align: left;
		min-height: 0;
	}
	.review-overlay {
		position: fixed;
		inset: 0;
		background: var(--bg);
		z-index: 15;
		display: flex;
		flex-direction: column;
		padding: 0.75rem;
		gap: 0.5rem;
	}
	.review-head {
		gap: 0.5rem;
	}
	.review-title {
		flex: 1;
	}
	.review-body {
		flex: 1;
		overflow-y: auto;
		min-height: 0;
	}
	.review-summary {
		font-size: 0.75rem;
	}
	.review-file {
		margin-bottom: 0.35rem;
	}
	.review-file summary {
		display: flex;
		gap: 0.4rem;
		align-items: baseline;
		cursor: pointer;
		padding: 0.3rem 0;
	}
	.review-file-status {
		font-weight: 700;
		font-size: 0.75rem;
	}
	.review-file-status.added {
		color: var(--ok, #4caf50);
	}
	.review-file-status.deleted {
		color: var(--danger);
	}
	.review-file-path {
		font-size: 0.8rem;
		word-break: break-all;
	}
	.review-nodiff {
		font-size: 0.75rem;
		padding-left: 1rem;
	}
	.diff-body {
		font-size: 0.68rem;
		line-height: 1.35;
		overflow-x: auto;
		background: var(--bg-elev-1);
		border: 1px solid var(--border);
		border-radius: var(--radius);
		padding: 0.5rem;
		white-space: pre;
	}
	.manage-projects {
		flex-shrink: 0;
		font-size: 0.75rem;
		padding: 0.2rem 0.5rem;
	}
	.project-chip.removing {
		border-style: dashed;
	}
	.chip-x {
		color: var(--danger);
		margin-right: 0.25rem;
	}
	.remove-overlay {
		position: fixed;
		inset: 0;
		background: rgb(0 0 0 / 55%);
		display: flex;
		align-items: center;
		justify-content: center;
		z-index: 20;
		padding: 1rem;
	}
	.remove-card {
		max-width: 26rem;
		width: 100%;
	}
	.remove-card p {
		font-size: 0.85rem;
		line-height: 1.4;
	}
	.remove-input {
		width: 100%;
		box-sizing: border-box;
	}
	.remove-actions {
		display: flex;
		justify-content: flex-end;
		gap: 0.6rem;
		margin-top: 0.75rem;
	}
	.remove-btn {
		background: var(--danger);
		color: var(--bg);
		border: none;
		border-radius: 6px;
		padding: 0.4rem 0.9rem;
		font-weight: 600;
	}
	.remove-btn:disabled {
		opacity: 0.4;
	}
	.scm-remote {
		font-size: 0.7rem;
		margin-top: 0.15rem;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
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
	.scm-commit-actions .suggesting {
		animation: sparkle-pulse 1s ease-in-out infinite;
	}
	@keyframes sparkle-pulse {
		50% {
			opacity: 0.3;
		}
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
	/* Badge first, title second: the title truncates, so a badge
	   after it would be the first thing ellipsised away on a phone
	   with the long auto-generated titles the coder writes. */
	.title-line {
		display: flex;
		align-items: center;
		gap: 0.3rem;
		min-width: 0;
		max-width: 100%;
	}
	.list-item-main strong {
		min-width: 0;
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
</style>
