<script lang="ts">
	// Model-picker popover for the coder panel.
	//
	// Two surfaces in one modal:
	//
	// 1. **The two slug fields** (Standard model / Cheap model) and
	//    the **Bill to** dropdown. These are the values the runner
	//    actually reads. Slugs are stored in their final
	//    `model:provider` form so the runner never has to concatenate.
	//
	// 2. **The catalog** — a scrollable list of model rows. Each row
	//    is collapsed by default and shows the model id + a short
	//    summary (max context, provider count, price range,
	//    throughput range). Click a row to expand it and reveal a
	//    per-provider table: context, in/out price, throughput, TTFT,
	//    plus a Pick button that writes
	//    `model.id:provider.provider` into the currently-edited tier.
	//    The `editingTier` toggle next to each model field decides
	//    which one a click targets.
	//
	// Provider auto-suffix / "default provider" / `:fastest`-style
	// synthetic suffixes are gone — the user picks the exact
	// `(model, provider)` pair they want, full stop. If they want a
	// synthetic suffix later, they can type it directly into the model
	// field; the picker doesn't try to be clever for them.
	//
	// Standard-model tier filtering: at the **model** level the list
	// pre-filters to `supports_tools_anywhere: true` because a model
	// with no tool-capable provider can't drive the agent loop. At the
	// **provider** level inside the expansion table, providers with
	// `supports_tools: false` are still listed (so the user can see
	// the full route surface) but their Pick button is disabled for
	// the standard tier — the loop would error out on the first
	// `tool_calls`.
	//
	// "Bill to" is unchanged: dropdown sourced from `identity.orgs`,
	// `slug ?? name` as the wire value, `name` as the display label.

	import { coder } from '../coder.svelte';
	import type { CoderModelSettings, RouterModel, RouterProvider } from '../protocol';
	import { onMount } from 'svelte';

	type Props = { onClose: () => void };
	let { onClose }: Props = $props();

	let standardModel = $state('');
	let cheapModel = $state('');
	let billTo = $state('');
	let saving = $state(false);
	let saveError = $state<string | null>(null);

	let modelSearch = $state('');
	let editingTier = $state<'standard' | 'cheap'>('standard');
	// Which model row is currently expanded — `null` for "all
	// collapsed". Single-expansion is on purpose: showing two
	// open provider tables at once is busy and the picker already
	// has the slug fields + bill-to competing for visual real
	// estate.
	let expandedModelId = $state<string | null>(null);

	onMount(async () => {
		await coder.loadModelSettings();
		if (coder.modelSettings) {
			standardModel = coder.modelSettings.standard_model;
			cheapModel = coder.modelSettings.cheap_model;
			billTo = coder.modelSettings.bill_to;
		}
		if (coder.routerModels === null) {
			void coder.loadModels();
		}
	});

	function modelMatches(model: RouterModel, needle: string, tier: 'standard' | 'cheap'): boolean {
		if (tier === 'standard' && !model.supports_tools_anywhere) {
			return false;
		}
		if (needle.length === 0) {
			return true;
		}
		if (model.id.toLowerCase().includes(needle) || model.owned_by.toLowerCase().includes(needle)) {
			return true;
		}
		return model.providers.some((p) => p.provider.toLowerCase().includes(needle));
	}

	const filteredModels = $derived.by(() => {
		const needle = modelSearch.trim().toLowerCase();
		return (coder.routerModels ?? []).filter((m) => modelMatches(m, needle, editingTier));
	});

	function currentTierSlug(): string {
		return editingTier === 'standard' ? standardModel : cheapModel;
	}

	function isPickedFor(model: RouterModel, provider: RouterProvider): boolean {
		return currentTierSlug() === `${model.id}:${provider.provider}`;
	}

	function pickProvider(model: RouterModel, provider: RouterProvider): void {
		const slug = `${model.id}:${provider.provider}`;
		if (editingTier === 'standard') {
			standardModel = slug;
		} else {
			cheapModel = slug;
		}
	}

	function toggleExpanded(modelId: string): void {
		expandedModelId = expandedModelId === modelId ? null : modelId;
	}

	function formatContext(tokens: number | null): string {
		if (tokens === null) {
			return '—';
		}
		if (tokens >= 1_000_000) {
			return `${(tokens / 1_000_000).toFixed(tokens % 1_000_000 === 0 ? 0 : 1)}M`;
		}
		if (tokens >= 1_000) {
			return `${Math.round(tokens / 1_000)}k`;
		}
		return `${tokens}`;
	}

	// Round to 3 decimals, then strip trailing zeros so `0.7` stays
	// `0.7` while `0.7499999999999999` (router's f64 artefact)
	// becomes `0.75` and `0.135` stays `0.135`. Prices are dollars
	// per million tokens so three decimals is overkill in 99% of
	// cases, but free to keep for the few three-decimal advertised
	// rates the router actually has (`0.135`, `0.269`).
	function formatPrice(n: number): string {
		return `$${parseFloat(n.toFixed(3))}`;
	}

	function formatPriceCell(pricing: { input: number; output: number } | null): string {
		if (pricing === null) {
			return '—';
		}
		return `${formatPrice(pricing.input)}/${formatPrice(pricing.output)}`;
	}

	function formatPriceRange(min: number, max: number): string {
		if (min === max) {
			return formatPrice(min);
		}
		return `${formatPrice(min)}–${formatPrice(max)}`;
	}

	function formatLatency(ms: number | null): string {
		if (ms === null) {
			return '—';
		}
		if (ms >= 1000) {
			return `${(ms / 1000).toFixed(1)}s`;
		}
		return `${Math.round(ms)}ms`;
	}

	function formatThroughput(tps: number | null): string {
		if (tps === null) {
			return '—';
		}
		if (tps >= 100) {
			return `${Math.round(tps)} tok/s`;
		}
		return `${tps.toFixed(1)} tok/s`;
	}

	// Header summary for a collapsed row. Returns the chips
	// (context, in price range, out price range, throughput) as a
	// flat object so the template doesn't re-walk the providers
	// list for each cell.
	function summaryFor(model: RouterModel): {
		context: string | null;
		priceIn: string | null;
		priceOut: string | null;
		throughput: string | null;
	} {
		const ctxs = model.providers.map((p) => p.context_length).filter((c): c is number => c !== null);
		const maxCtx = ctxs.length > 0 ? Math.max(...ctxs) : null;

		const inPrices = model.providers.map((p) => p.pricing?.input ?? null).filter((v): v is number => v !== null);
		const outPrices = model.providers.map((p) => p.pricing?.output ?? null).filter((v): v is number => v !== null);

		const tpsList = model.providers.map((p) => p.throughput).filter((t): t is number => t !== null);
		const maxTps = tpsList.length > 0 ? Math.max(...tpsList) : null;

		return {
			context: maxCtx === null ? null : `${formatContext(maxCtx)} ctx`,
			priceIn: inPrices.length === 0 ? null : `${formatPriceRange(Math.min(...inPrices), Math.max(...inPrices))}/M in`,
			priceOut:
				outPrices.length === 0 ? null : `${formatPriceRange(Math.min(...outPrices), Math.max(...outPrices))}/M out`,
			throughput: maxTps === null ? null : `up to ${formatThroughput(maxTps)}`,
		};
	}

	// Resolve a wire slug (`owner/name` or `owner/name:provider`) to a
	// catalog entry. Returns the matched `RouterModel` together with
	// the specific provider when the slug carried a `:provider`
	// suffix that actually exists on the model. Synthetic suffixes
	// (`:fastest`, `:cheapest`, `:preferred`) or typos fall through
	// with `provider: null` so the caller renders the model-wide
	// fallback; same shape as a bare slug.
	function resolveSlug(slug: string): { model: RouterModel; provider: RouterProvider | null } | null {
		const trimmed = slug.trim();
		if (trimmed.length === 0 || coder.routerModels === null) {
			return null;
		}
		const colon = trimmed.indexOf(':');
		const modelId = colon === -1 ? trimmed : trimmed.slice(0, colon);
		const providerName = colon === -1 ? null : trimmed.slice(colon + 1);
		const model = coder.routerModels.find((m) => m.id === modelId);
		if (!model) {
			return null;
		}
		if (providerName === null) {
			return { model, provider: null };
		}
		const provider = model.providers.find((p) => p.provider === providerName) ?? null;
		return { model, provider };
	}

	// One-line "what does this slug get me" string. Shown under each
	// model field next to the hint copy. `null` when the slug doesn't
	// match anything in the catalog (custom string, catalog not yet
	// fetched) — caller falls back to the plain hint in that case.
	function slugDetails(slug: string): string | null {
		const hit = resolveSlug(slug);
		if (hit === null) {
			return null;
		}
		// Pick the "representative" provider whose numbers we'll show:
		// the explicit one when the slug pinned it, else the one with
		// the best throughput (tie-broken by lowest input price), else
		// the first provider with any pricing, else give up.
		let provider: RouterProvider | null = hit.provider;
		let tag: string;
		if (provider !== null) {
			tag = `(${provider.provider})`;
		} else {
			provider = pickRepresentative(hit.model);
			if (provider === null) {
				return null;
			}
			tag = `(via ${provider.provider})`;
		}
		const parts: string[] = [];
		if (provider.context_length !== null) {
			parts.push(`${formatContext(provider.context_length)} ctx`);
		}
		if (provider.pricing !== null) {
			parts.push(`${formatPrice(provider.pricing.input)}/${formatPrice(provider.pricing.output)} per M`);
		}
		if (provider.throughput !== null) {
			parts.push(formatThroughput(provider.throughput));
		}
		if (parts.length === 0) {
			return null;
		}
		return `${parts.join(' · ')} ${tag}`;
	}

	function pickRepresentative(model: RouterModel): RouterProvider | null {
		const withTps = model.providers.filter((p) => p.throughput !== null);
		if (withTps.length > 0) {
			return withTps.reduce((best, p) => {
				if (best.throughput === null) {
					return p;
				}
				if (p.throughput === null) {
					return best;
				}
				if (p.throughput > best.throughput) {
					return p;
				}
				if (p.throughput === best.throughput) {
					const bestIn = best.pricing?.input ?? Number.POSITIVE_INFINITY;
					const pIn = p.pricing?.input ?? Number.POSITIVE_INFINITY;
					return pIn < bestIn ? p : best;
				}
				return best;
			});
		}
		const withPricing = model.providers.find((p) => p.pricing !== null);
		return withPricing ?? null;
	}

	const standardDetails = $derived(slugDetails(standardModel));
	const cheapDetails = $derived(slugDetails(cheapModel));

	// Orgs we surface in the bill-to dropdown: only the ones the user
	// explicitly authorized moon-ide for at the OAuth consent screen.
	// HF flags those by emitting a `roleInOrg` value; an org the user
	// is a member of but didn't tick stays in the userinfo response
	// with a `null` role and is filtered here — we have no signal
	// about it. The "Personal account" option (rendered separately
	// below) is always available regardless of org consent.
	const orgs = $derived((coder.status?.identity?.orgs ?? []).filter((o) => o.role_in_org !== null));

	async function onSave(): Promise<void> {
		saving = true;
		saveError = null;
		const next: CoderModelSettings = {
			standard_model: standardModel.trim(),
			cheap_model: cheapModel.trim(),
			bill_to: billTo.trim(),
		};
		try {
			await coder.saveModelSettings(next);
			onClose();
		} catch (err) {
			saveError = err instanceof Error ? err.message : String(err);
		} finally {
			saving = false;
		}
	}

	function onCancel(): void {
		onClose();
	}

	function onBackdropClick(e: MouseEvent): void {
		if (e.target === e.currentTarget) {
			onClose();
		}
	}

	function onKeydown(e: KeyboardEvent): void {
		if (e.key === 'Escape') {
			e.stopPropagation();
			onClose();
		}
	}
</script>

<div
	class="overlay"
	role="dialog"
	aria-modal="true"
	aria-label="Coder model settings"
	tabindex="-1"
	onclick={onBackdropClick}
	onkeydown={onKeydown}
>
	<div class="card">
		<header>
			<h2>Coder model settings</h2>
			<button type="button" class="close" aria-label="Close" onclick={onCancel}>×</button>
		</header>

		<section class="fields">
			<label class="field">
				<span class="label-row">
					<span class="label-name">Standard model</span>
					<button
						type="button"
						class="tier-tab"
						class:active={editingTier === 'standard'}
						onclick={() => (editingTier = 'standard')}>edit</button
					>
				</span>
				<input
					type="text"
					bind:value={standardModel}
					placeholder="Qwen/Qwen3.5-397B-A17B:scaleway"
					spellcheck="false"
					autocomplete="off"
				/>
				<span class="hint">
					Drives the main agent loop and every sub-agent. Empty = built-in default.
					{#if standardDetails !== null}
						<span class="stats">· {standardDetails}</span>
					{/if}
				</span>
			</label>

			<label class="field">
				<span class="label-row">
					<span class="label-name">Cheap model</span>
					<button
						type="button"
						class="tier-tab"
						class:active={editingTier === 'cheap'}
						onclick={() => (editingTier = 'cheap')}>edit</button
					>
				</span>
				<input
					type="text"
					bind:value={cheapModel}
					placeholder="Qwen/Qwen3-Coder-30B-A3B-Instruct:scaleway"
					spellcheck="false"
					autocomplete="off"
				/>
				<span class="hint">
					Used for commit messages, branch names, compaction summaries, folder summaries.
					{#if cheapDetails !== null}
						<span class="stats">· {cheapDetails}</span>
					{/if}
				</span>
			</label>

			<label class="field">
				<span class="label-row">
					<span class="label-name">Bill to</span>
				</span>
				<select class="bill-to" bind:value={billTo}>
					<option value="">Personal account</option>
					{#each orgs as org (org.slug ?? org.name)}
						<option value={org.slug ?? org.name} disabled={!org.can_pay}>
							{org.name}
							{#if org.slug !== null && org.slug !== org.name}
								({org.slug})
							{/if}
							{#if org.is_enterprise}— enterprise{/if}
							{#if !org.can_pay}— can't pay{/if}
						</option>
					{/each}
				</select>
				<span class="hint">
					Sent as <code>X-HF-Bill-To</code>. Personal = your own HF account. Orgs you've authorized moon-ide for show up
					here; ones that can't pay are disabled. If an org you expect is missing, sign out and back in and tick it at
					the OAuth consent screen.
				</span>
			</label>
		</section>

		<section class="catalog">
			<header class="catalog-header">
				<span class="catalog-title">
					Catalog ({editingTier} model)
				</span>
				<input
					type="search"
					bind:value={modelSearch}
					placeholder="Filter by name, owner, or provider…"
					spellcheck="false"
					autocomplete="off"
				/>
			</header>
			{#if coder.modelsLoading && coder.routerModels === null}
				<p class="catalog-hint">Loading models from <code>router.huggingface.co</code>…</p>
			{:else if coder.routerModels === null && coder.modelsError !== null}
				<p class="error">{coder.modelsError}</p>
				<button type="button" class="secondary" onclick={() => coder.loadModels()}>Retry</button>
			{:else if filteredModels.length === 0}
				<p class="catalog-hint">No models match this filter.</p>
			{:else}
				<ul class="catalog-list">
					{#each filteredModels as model (model.id)}
						{@const expanded = expandedModelId === model.id}
						{@const summary = summaryFor(model)}
						<li class="model-li" class:expanded>
							<button type="button" class="model-row" aria-expanded={expanded} onclick={() => toggleExpanded(model.id)}>
								<span class="chevron" aria-hidden="true">{expanded ? '▾' : '▸'}</span>
								<span class="model-id">{model.id}</span>
								{#if !model.supports_tools_anywhere}
									<span class="no-tools" title="No provider exposes tool calls — won't work as the standard model">
										no tools
									</span>
								{/if}
								<span class="model-summary">
									{#if summary.context}<span>{summary.context}</span>{/if}
									<span>{model.providers.length} provider{model.providers.length === 1 ? '' : 's'}</span>
									{#if summary.priceIn}<span>{summary.priceIn}</span>{/if}
									{#if summary.priceOut}<span>{summary.priceOut}</span>{/if}
									{#if summary.throughput}<span class="perf">{summary.throughput}</span>{/if}
								</span>
							</button>
							{#if expanded}
								<div class="provider-table-wrap">
									<table class="provider-table">
										<thead>
											<tr>
												<th scope="col">Provider</th>
												<th scope="col">Context</th>
												<th scope="col">$ in / out per M</th>
												<th scope="col">Throughput</th>
												<th scope="col">TTFT</th>
												<th scope="col" class="pick-col"></th>
											</tr>
										</thead>
										<tbody>
											{#each model.providers as provider (provider.provider)}
												{@const picked = isPickedFor(model, provider)}
												{@const disabled = editingTier === 'standard' && !provider.supports_tools}
												<tr class="provider-row" class:picked class:disabled>
													<td class="provider-name">
														{provider.provider}
														{#if !provider.supports_tools}
															<span class="cell-flag" title="No tool calls on this route">no tools</span>
														{/if}
													</td>
													<td>{formatContext(provider.context_length)}</td>
													<td>{formatPriceCell(provider.pricing)}</td>
													<td>{formatThroughput(provider.throughput)}</td>
													<td>{formatLatency(provider.first_token_latency_ms)}</td>
													<td class="pick-col">
														<button
															type="button"
															class="pick"
															class:picked
															onclick={() => pickProvider(model, provider)}
															{disabled}
															title={disabled
																? 'Standard tier requires tool-capable providers'
																: picked
																	? `Currently picked for ${editingTier} tier`
																	: `Pick for ${editingTier} tier`}
														>
															{picked ? 'Picked' : 'Pick'}
														</button>
													</td>
												</tr>
											{/each}
										</tbody>
									</table>
								</div>
							{/if}
						</li>
					{/each}
				</ul>
			{/if}
		</section>

		<footer>
			{#if saveError}
				<p class="error">{saveError}</p>
			{/if}
			<div class="footer-actions">
				<button type="button" class="secondary" onclick={onCancel} disabled={saving}>Cancel</button>
				<button type="button" class="primary" onclick={onSave} disabled={saving}>
					{saving ? 'Saving…' : 'Save'}
				</button>
			</div>
		</footer>
	</div>
</div>

<style>
	.overlay {
		position: fixed;
		inset: 0;
		background: rgba(0, 0, 0, 0.55);
		display: flex;
		align-items: center;
		justify-content: center;
		z-index: 50;
	}
	.card {
		background: var(--m-bg-1);
		border: 1px solid var(--m-border);
		border-radius: 8px;
		padding: 18px 22px 20px;
		max-width: 820px;
		width: calc(100% - 32px);
		max-height: calc(100vh - 32px);
		display: flex;
		flex-direction: column;
		gap: 14px;
		box-shadow: 0 12px 36px rgba(0, 0, 0, 0.55);
	}
	header {
		display: flex;
		align-items: center;
		justify-content: space-between;
	}
	h2 {
		font-size: 14px;
		font-weight: 600;
		margin: 0;
		color: var(--m-fg);
	}
	.close {
		background: transparent;
		border: 0;
		color: var(--m-fg-muted);
		cursor: pointer;
		font-size: 18px;
		line-height: 1;
		padding: 0 4px;
	}
	.close:hover {
		color: var(--m-fg);
	}
	.fields {
		display: flex;
		flex-direction: column;
		gap: 10px;
	}
	.field {
		display: flex;
		flex-direction: column;
		gap: 4px;
	}
	.label-row {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 8px;
	}
	.label-name {
		font-size: 12px;
		font-weight: 600;
		color: var(--m-fg);
	}
	.tier-tab {
		background: transparent;
		border: 1px solid var(--m-border);
		border-radius: 4px;
		color: var(--m-fg-muted);
		font-size: 10px;
		text-transform: uppercase;
		letter-spacing: 0.5px;
		padding: 2px 6px;
		cursor: pointer;
	}
	.tier-tab.active {
		background: var(--m-accent);
		border-color: var(--m-accent);
		color: var(--m-on-accent, #fff);
	}
	.field input,
	.field select {
		background: var(--m-bg-overlay);
		border: 1px solid var(--m-border);
		border-radius: 4px;
		color: var(--m-fg);
		font-family: var(--m-font-mono, ui-monospace, monospace);
		font-size: 12px;
		padding: 6px 8px;
	}
	/* WebKitGTK paints native `<select>` fields with the OS theme,
	   which on a dark Linux desktop with a light system theme leaves
	   us a glaring white pill — `color-scheme: dark` on `:root`
	   fixes the popup, this strips the native arrow and supplies our
	   own to keep the field flush with our `<input>` look. */
	.field select {
		appearance: none;
		font-family: var(--m-font, system-ui, sans-serif);
		padding-right: 28px;
		background-image: url("data:image/svg+xml;utf8,<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 12 12'><path fill='none' stroke='%238a93ab' stroke-width='1.5' stroke-linecap='round' stroke-linejoin='round' d='M3 4.5l3 3 3-3'/></svg>");
		background-repeat: no-repeat;
		background-position: right 8px center;
		background-size: 12px 12px;
	}
	.field input:focus,
	.field select:focus {
		outline: none;
		border-color: var(--m-accent);
	}
	.hint {
		font-size: 11px;
		color: var(--m-fg-muted);
		line-height: 1.4;
	}
	.hint code {
		font-size: 10.5px;
		padding: 1px 4px;
		border-radius: 3px;
		background: var(--m-bg-overlay);
	}
	/* The catalog-resolved stats segment of the hint line. Same row
	   as the hint sentence but rendered in the regular foreground so
	   the numbers don't blend into the explanatory copy. */
	.hint .stats {
		color: var(--m-fg);
		font-family: var(--m-font-mono, ui-monospace, monospace);
		font-size: 10.5px;
	}

	.catalog {
		display: flex;
		flex-direction: column;
		gap: 6px;
		min-height: 0;
		flex: 1;
	}
	.catalog-header {
		display: flex;
		align-items: center;
		gap: 8px;
		justify-content: space-between;
	}
	.catalog-title {
		font-size: 11px;
		text-transform: uppercase;
		letter-spacing: 0.5px;
		color: var(--m-fg-muted);
	}
	.catalog-header input {
		flex: 1;
		max-width: 240px;
		background: var(--m-bg-overlay);
		border: 1px solid var(--m-border);
		border-radius: 4px;
		color: var(--m-fg);
		font-size: 12px;
		padding: 4px 8px;
	}
	.catalog-hint {
		margin: 0;
		font-size: 11px;
		color: var(--m-fg-muted);
	}

	.catalog-list {
		list-style: none;
		margin: 0;
		padding: 0;
		overflow-y: auto;
		max-height: 360px;
		border: 1px solid var(--m-border);
		border-radius: 4px;
		background: var(--m-bg-overlay);
	}
	.model-li {
		border-bottom: 1px solid var(--m-border);
	}
	.model-li:last-child {
		border-bottom: 0;
	}
	.model-row {
		display: flex;
		align-items: center;
		gap: 8px;
		width: 100%;
		text-align: left;
		background: transparent;
		border: 0;
		color: var(--m-fg);
		font-size: 12px;
		padding: 6px 10px;
		cursor: pointer;
	}
	.model-row:hover {
		background: var(--m-bg-2, var(--m-bg-1));
	}
	.model-li.expanded > .model-row {
		background: var(--m-bg-2, var(--m-bg-1));
	}
	.chevron {
		display: inline-block;
		width: 12px;
		color: var(--m-fg-muted);
		font-size: 10px;
		flex-shrink: 0;
	}
	.model-id {
		font-family: var(--m-font-mono, ui-monospace, monospace);
		font-size: 12px;
		color: var(--m-fg);
		flex-shrink: 0;
	}
	.no-tools {
		font-size: 9.5px;
		color: var(--m-warning, #f0b86e);
		text-transform: uppercase;
		letter-spacing: 0.5px;
		flex-shrink: 0;
	}
	.model-summary {
		display: flex;
		gap: 10px;
		flex-wrap: wrap;
		font-size: 10.5px;
		color: var(--m-fg-muted);
		margin-left: auto;
	}
	.model-summary .perf {
		color: var(--m-success);
	}

	.provider-table-wrap {
		padding: 0 10px 8px 30px;
		overflow-x: auto;
	}
	.provider-table {
		width: 100%;
		border-collapse: collapse;
		font-size: 11px;
		color: var(--m-fg);
	}
	.provider-table thead th {
		text-align: left;
		font-weight: 500;
		font-size: 10px;
		text-transform: uppercase;
		letter-spacing: 0.5px;
		color: var(--m-fg-muted);
		padding: 4px 8px;
		border-bottom: 1px solid var(--m-border);
	}
	.provider-table tbody td {
		padding: 5px 8px;
		border-bottom: 1px solid var(--m-border);
		font-family: var(--m-font-mono, ui-monospace, monospace);
		font-size: 11px;
	}
	.provider-table tbody tr:last-child td {
		border-bottom: 0;
	}
	.provider-table tbody tr.disabled td {
		color: var(--m-fg-subtle);
	}
	.provider-table tbody tr.picked td {
		background: rgba(126, 163, 255, 0.08);
	}
	.provider-name {
		color: var(--m-fg);
		font-weight: 500;
		font-family: var(--m-font-ui, system-ui, sans-serif) !important;
	}
	.cell-flag {
		display: inline-block;
		margin-left: 6px;
		font-size: 9px;
		font-family: var(--m-font-ui, system-ui, sans-serif);
		color: var(--m-warning, #f0b86e);
		text-transform: uppercase;
		letter-spacing: 0.5px;
	}
	.pick-col {
		text-align: right;
		white-space: nowrap;
	}
	.pick {
		background: transparent;
		border: 1px solid var(--m-border);
		border-radius: 4px;
		color: var(--m-fg);
		font-size: 10.5px;
		padding: 2px 10px;
		cursor: pointer;
	}
	.pick:hover:not([disabled]) {
		background: var(--m-bg-3, var(--m-bg-2));
		border-color: var(--m-border-strong, var(--m-border));
	}
	.pick.picked {
		background: var(--m-accent);
		border-color: var(--m-accent);
		color: var(--m-on-accent, #fff);
	}
	.pick[disabled] {
		opacity: 0.45;
		cursor: not-allowed;
	}

	footer {
		display: flex;
		flex-direction: column;
		gap: 8px;
	}
	.footer-actions {
		display: flex;
		justify-content: flex-end;
		gap: 8px;
	}
	.primary,
	.secondary {
		border-radius: 4px;
		padding: 6px 14px;
		font-size: 12px;
		cursor: pointer;
	}
	.primary {
		background: var(--m-accent);
		border: 1px solid var(--m-accent);
		color: var(--m-on-accent, #fff);
	}
	.primary[disabled] {
		opacity: 0.6;
		cursor: progress;
	}
	.secondary {
		background: transparent;
		border: 1px solid var(--m-border);
		color: var(--m-fg);
	}
	.error {
		margin: 0;
		font-size: 11px;
		color: var(--m-error, #d34c4c);
	}
</style>
