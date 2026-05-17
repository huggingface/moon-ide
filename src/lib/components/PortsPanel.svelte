<script lang="ts">
	import { openUrl } from '@tauri-apps/plugin-opener';

	import { ports } from '../ports.svelte';
	import type { ForwardedPort, ForwardedPortHealth } from '../protocol';

	// Body for the `kind: 'ports'` bottom-panel tab.
	//
	// Surface: rows of `[label] [container] -> [host] [status dot]
	// [open in browser] [×]`, plus an inline "Add forward" row that
	// only commits to disk on Apply. Status dots come from
	// `ports.status`, which the store keeps current via the
	// `ports:state` Tauri event.
	//
	// We deliberately don't auto-commit per keystroke. The picker
	// reads from the persisted set on mount and writes back the
	// whole set on Apply — same shape as the IPC command —
	// because port forwarding is an "I want exactly these N
	// forwards" operation and partial edits have surprising
	// effects on the proxy sidecar (every commit recreates it).

	type Draft = {
		container_port: string;
		host_port: string;
		label: string;
	};

	function draftFromForward(f: ForwardedPort): Draft {
		return {
			container_port: String(f.container_port),
			host_port: String(f.host_port),
			label: f.label,
		};
	}

	let drafts = $state<Draft[]>([]);
	let addDraft = $state<Draft>({ container_port: '', host_port: '', label: '' });
	let formError = $state<string | null>(null);

	const liveForwards = $derived(ports.forwards);

	$effect(() => {
		// Re-seed the drafts from the live set whenever the
		// store's forwarded list changes. Without this an external
		// edit (another window, the test plan's `ports_set` curl
		// example) wouldn't show up in the picker.
		drafts = liveForwards.map(draftFromForward);
	});

	function setDraftField(idx: number, key: keyof Draft, value: string) {
		const current = drafts[idx];
		if (!current) {
			return;
		}
		drafts[idx] = { ...current, [key]: value };
	}

	function removeDraft(idx: number) {
		drafts = drafts.toSpliced(idx, 1);
	}

	async function applyAdd() {
		const parsed = parseDraft(addDraft);
		if (!parsed) {
			return;
		}
		drafts = [...drafts, draftFromForward(parsed)];
		addDraft = { container_port: '', host_port: '', label: '' };
		await commit();
	}

	async function commit() {
		formError = null;
		const parsedRows: ForwardedPort[] = [];
		for (const d of drafts) {
			const row = parseDraft(d);
			if (!row) {
				return;
			}
			parsedRows.push(row);
		}
		const seen = new Set<number>();
		for (const row of parsedRows) {
			if (seen.has(row.host_port)) {
				formError = `Duplicate host port ${row.host_port}.`;
				return;
			}
			seen.add(row.host_port);
		}
		try {
			await ports.commit(parsedRows);
		} catch {
			// `ports.commit` already wrote `lastError`; the panel
			// reads it from the store.
		}
	}

	function parseDraft(d: Draft): ForwardedPort | null {
		const cp = parsePort(d.container_port, 'container');
		const hp = parsePort(d.host_port || d.container_port, 'host');
		if (!cp || !hp) {
			return null;
		}
		return { container_port: cp, host_port: hp, label: d.label.trim() };
	}

	function parsePort(raw: string, kind: 'host' | 'container'): number | null {
		const trimmed = raw.trim();
		if (trimmed.length === 0) {
			formError = `Enter a ${kind} port.`;
			return null;
		}
		const n = Number(trimmed);
		if (!Number.isInteger(n) || n <= 0 || n > 65535) {
			formError = `${kind} port must be an integer between 1 and 65535.`;
			return null;
		}
		return n;
	}

	function dotClass(health: ForwardedPortHealth | null): string {
		if (health === 'live') {
			return 'dot dot-live';
		}
		if (health === 'host_port_busy') {
			return 'dot dot-busy';
		}
		if (health === 'proxy_down') {
			return 'dot dot-down';
		}
		return 'dot dot-pending';
	}

	function healthLabel(health: ForwardedPortHealth | null): string {
		if (health === 'live') {
			return 'Forward is live';
		}
		if (health === 'host_port_busy') {
			return 'Host port is busy on the host (something else is bound)';
		}
		if (health === 'proxy_down') {
			return 'Proxy sidecar is not running — start the workspace shell';
		}
		return 'Pending';
	}

	function openInBrowser(host_port: number) {
		void openUrl(`http://localhost:${host_port}`);
	}
</script>

<div class="panel">
	<header class="title">
		<h2>Workspace port forwards</h2>
		<p class="hint">
			Bind <code>127.0.0.1:&lt;host&gt;</code> on this machine to <code>&lt;container&gt;</code> inside the workspace
			dev container. Edits never recreate <code>dev</code> — only the proxy sidecar.
		</p>
	</header>
	{#if drafts.length === 0}
		<p class="empty">No forwards declared. Add one below.</p>
	{:else}
		<table>
			<thead>
				<tr>
					<th>Label</th>
					<th>Container</th>
					<th>Host</th>
					<th></th>
					<th></th>
				</tr>
			</thead>
			<tbody>
				{#each drafts as draft, idx (idx)}
					{@const hostNum = Number(draft.host_port)}
					{@const health = Number.isFinite(hostNum) ? ports.healthFor(hostNum) : null}
					<tr>
						<td>
							<input
								type="text"
								class="cell"
								placeholder="vite"
								value={draft.label}
								oninput={(e) => setDraftField(idx, 'label', e.currentTarget.value)}
							/>
						</td>
						<td>
							<input
								type="text"
								class="cell port"
								inputmode="numeric"
								placeholder="3000"
								value={draft.container_port}
								oninput={(e) => setDraftField(idx, 'container_port', e.currentTarget.value)}
							/>
						</td>
						<td>
							<input
								type="text"
								class="cell port"
								inputmode="numeric"
								placeholder={draft.container_port || '3000'}
								value={draft.host_port}
								oninput={(e) => setDraftField(idx, 'host_port', e.currentTarget.value)}
							/>
						</td>
						<td class="status-cell">
							<span class={dotClass(health)} title={healthLabel(health)}></span>
							<button
								type="button"
								class="link"
								disabled={health !== 'live'}
								title="Open http://localhost:{draft.host_port} in your browser"
								onclick={() => openInBrowser(Number(draft.host_port))}>open</button
							>
						</td>
						<td class="actions-cell">
							<button type="button" class="x" aria-label="Remove" onclick={() => removeDraft(idx)}>×</button>
						</td>
					</tr>
				{/each}
			</tbody>
		</table>
	{/if}

	<form
		class="add-row"
		onsubmit={(e) => {
			e.preventDefault();
			void applyAdd();
		}}
	>
		<input
			type="text"
			class="cell"
			placeholder="Label (optional)"
			value={addDraft.label}
			oninput={(e) => (addDraft.label = e.currentTarget.value)}
		/>
		<input
			type="text"
			class="cell port"
			inputmode="numeric"
			placeholder="Container port (e.g. 3000)"
			value={addDraft.container_port}
			oninput={(e) => (addDraft.container_port = e.currentTarget.value)}
		/>
		<input
			type="text"
			class="cell port"
			inputmode="numeric"
			placeholder="Host port (defaults to container)"
			value={addDraft.host_port}
			oninput={(e) => (addDraft.host_port = e.currentTarget.value)}
		/>
		<button type="submit" class="primary" disabled={ports.busy}>Add forward</button>
	</form>

	<div class="apply-row">
		<button type="button" class="primary" disabled={ports.busy} onclick={() => void commit()}>
			{ports.busy ? 'Applying…' : 'Apply'}
		</button>
		{#if formError}<span class="form-error">{formError}</span>{/if}
		{#if ports.lastError}<span class="form-error">{ports.lastError}</span>{/if}
	</div>

	{#if ports.conflicts.length > 0}
		<p class="conflict">
			Host port already in use:
			{#each ports.conflicts as c, i (c.host_port)}
				<code>{c.host_port}</code>{i < ports.conflicts.length - 1 ? ', ' : ''}
			{/each}
			. Pick a different host port and re-apply.
		</p>
	{/if}
</div>

<style>
	.panel {
		flex: 1;
		min-height: 0;
		display: flex;
		flex-direction: column;
		gap: 10px;
		padding: 10px 14px;
		font-size: 12px;
		color: var(--m-fg);
		overflow: auto;
	}
	.title {
		display: flex;
		flex-direction: column;
		gap: 2px;
	}
	.title h2 {
		margin: 0;
		font-size: 13px;
		color: var(--m-fg);
	}
	.hint {
		margin: 0;
		color: var(--m-fg-muted);
		line-height: 1.4;
	}
	.empty {
		margin: 0;
		color: var(--m-fg-muted);
	}
	table {
		width: 100%;
		border-collapse: collapse;
	}
	th {
		text-align: left;
		font-weight: 500;
		padding: 4px 8px 4px 0;
		color: var(--m-fg-muted);
		border-bottom: 1px solid var(--m-border);
	}
	td {
		padding: 4px 8px 4px 0;
		vertical-align: middle;
	}
	.status-cell {
		display: flex;
		align-items: center;
		gap: 8px;
	}
	.actions-cell {
		text-align: right;
	}
	.cell {
		font: inherit;
		width: 100%;
		max-width: 220px;
		padding: 4px 6px;
		background: var(--m-bg);
		color: var(--m-fg);
		border: 1px solid var(--m-border);
		border-radius: 3px;
	}
	.cell.port {
		max-width: 110px;
		font-variant-numeric: tabular-nums;
	}
	.cell:focus {
		outline: 1px solid var(--m-accent);
		outline-offset: -1px;
	}
	.dot {
		display: inline-block;
		width: 8px;
		height: 8px;
		border-radius: 50%;
		background: var(--m-fg-subtle);
	}
	.dot-live {
		background: var(--m-success, #6ec48a);
	}
	.dot-busy {
		background: var(--m-error, #d96d6d);
	}
	.dot-down {
		background: var(--m-warning, #d8a657);
	}
	.dot-pending {
		background: var(--m-fg-subtle);
	}
	.link {
		font: inherit;
		background: transparent;
		border: none;
		padding: 0;
		color: var(--m-accent);
		cursor: pointer;
		text-decoration: underline;
	}
	.link:disabled {
		color: var(--m-fg-subtle);
		cursor: not-allowed;
		text-decoration: none;
	}
	.x {
		font: inherit;
		display: inline-flex;
		align-items: center;
		justify-content: center;
		width: 18px;
		height: 18px;
		background: transparent;
		border: none;
		color: var(--m-fg-subtle);
		font-size: 14px;
		cursor: pointer;
		border-radius: 3px;
	}
	.x:hover {
		background: var(--m-bg-1);
		color: var(--m-fg);
	}
	.add-row {
		display: flex;
		gap: 6px;
		flex-wrap: wrap;
		align-items: center;
		padding-top: 8px;
		border-top: 1px solid var(--m-border);
	}
	.apply-row {
		display: flex;
		align-items: center;
		gap: 12px;
	}
	.primary {
		font: inherit;
		padding: 4px 10px;
		background: var(--m-accent);
		color: var(--m-accent-fg, #fff);
		border: 1px solid var(--m-accent);
		border-radius: 3px;
		cursor: pointer;
	}
	.primary:disabled {
		background: var(--m-bg-1);
		color: var(--m-fg-muted);
		border-color: var(--m-border);
		cursor: not-allowed;
	}
	.form-error {
		color: var(--m-error, #d96d6d);
	}
	.conflict {
		margin: 0;
		color: var(--m-warning, #d8a657);
	}
	code {
		font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
	}
</style>
