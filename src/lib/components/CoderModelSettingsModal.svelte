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
	import type { CoderModelSettings, CoderProviderConfig, ProviderKind, RouterModel, RouterProvider } from '../protocol';
	import { onMount } from 'svelte';

	// Built-in provider presets surfaced in the `+ Add provider`
	// row. Each preset locks the base URL, threads its `kind`
	// through the probe / save commands so the backend hits the
	// right wire shape, and links to the upstream key dashboard.
	// The list lives next to the modal because it's pure UI
	// chrome — the runner-side code is keyed off
	// `CoderProviderConfig.kind`, not these labels.
	type BuiltinPreset = {
		kind: ProviderKind;
		label: string;
		base_url: string;
		api_key_placeholder: string;
		api_key_dashboard_url: string;
		api_key_dashboard_label: string;
	};
	const BUILTIN_PRESETS: readonly BuiltinPreset[] = [
		{
			kind: 'open_router',
			label: 'OpenRouter',
			base_url: 'https://openrouter.ai/api/v1',
			api_key_placeholder: 'sk-or-...',
			api_key_dashboard_url: 'https://openrouter.ai/settings/keys',
			api_key_dashboard_label: 'openrouter.ai/settings/keys',
		},
		{
			kind: 'anthropic',
			label: 'Anthropic',
			base_url: 'https://api.anthropic.com',
			api_key_placeholder: 'sk-ant-...',
			api_key_dashboard_url: 'https://console.anthropic.com/settings/keys',
			api_key_dashboard_label: 'console.anthropic.com/settings/keys',
		},
	];

	function presetForKind(kind: ProviderKind): BuiltinPreset | null {
		return BUILTIN_PRESETS.find((p) => p.kind === kind) ?? null;
	}

	type Props = { onClose: () => void };
	let { onClose }: Props = $props();

	// Picker state. `standardModel` / `cheapModel` / `billTo` are
	// the picks for whichever provider is currently active in the
	// local edit state — when the user flips `activeProviderId`,
	// we commit the textbox values back to the previous provider's
	// record (or the HF slot) and load the new provider's picks
	// into the textboxes. Save flow rolls all of that into a
	// single `CoderModelSettings` write.
	let standardModel = $state('');
	let cheapModel = $state('');
	let billTo = $state('');
	let activeProviderId = $state<string | null>(null);
	let providers = $state<CoderProviderConfig[]>([]);
	let saving = $state(false);
	let saveError = $state<string | null>(null);

	let modelSearch = $state('');
	let editingTier = $state<'standard' | 'cheap'>('standard');

	// Add / edit provider sub-form state. `null` = closed; a
	// non-null value means the inline form is open. `id` is
	// pre-allocated from `coder.newProviderId()` for new entries
	// so the keyring slot is addressable from the moment the
	// user types a key.
	type ProviderDraft = {
		id: string;
		label: string;
		kind: ProviderKind;
		base_url: string;
		api_key: string; // local-only; never read back from the keyring
		is_new: boolean;
	};
	let providerDraft = $state<ProviderDraft | null>(null);
	let probing = $state(false);
	let probeMessage = $state<string | null>(null);
	let probeError = $state<string | null>(null);

	// Web-search section state. The Tavily key itself is held
	// only in the keyring; here we just track whether one is set
	// (loaded asynchronously on mount) and a draft string the user
	// is typing in. The draft is discarded the moment the modal
	// closes — keys never round-trip back through `coder_get_*`.
	let webKeyDraft = $state('');
	let webKeySaving = $state(false);
	let webKeyError = $state<string | null>(null);
	// Which model row is currently expanded — `null` for "all
	// collapsed". Single-expansion is on purpose: showing two
	// open provider tables at once is busy and the picker already
	// has the slug fields + bill-to competing for visual real
	// estate.
	let expandedModelId = $state<string | null>(null);

	// Mirror of the runner-side `is_local_base_url` heuristic so
	// the picker can decide whether to show the "no key" badge
	// without bothering the user about a localhost server they
	// intentionally run keyless.
	function isLocalUrl(url: string): boolean {
		const afterScheme = url.includes('://') ? url.slice(url.indexOf('://') + 3) : url;
		const hostEnd = afterScheme.search(/[/:?#]/);
		const host = hostEnd === -1 ? afterScheme : afterScheme.slice(0, hostEnd);
		return host === 'localhost' || host === '127.0.0.1' || host === '::1' || host.endsWith('.local');
	}

	function cloneProviders(list: CoderProviderConfig[]): CoderProviderConfig[] {
		// In-place spread inside `.map` is flagged by `oxlint`'s
		// `no-map-spread` rule. `Object.assign` produces the same
		// shallow clone without the per-iteration heap allocation
		// the rule cares about.
		const out: CoderProviderConfig[] = [];
		for (const item of list) {
			out.push(Object.assign({}, item));
		}
		return out;
	}

	function loadFromSettings(settings: CoderModelSettings): void {
		providers = cloneProviders(settings.providers);
		activeProviderId = settings.active_provider;
		if (activeProviderId === null) {
			standardModel = settings.standard_model;
			cheapModel = settings.cheap_model;
			billTo = settings.bill_to;
		} else {
			const entry = providers.find((p) => p.id === activeProviderId);
			if (entry) {
				standardModel = entry.standard_model;
				cheapModel = entry.cheap_model;
			}
			billTo = settings.bill_to;
		}
	}

	onMount(async () => {
		await coder.loadModelSettings();
		if (coder.modelSettings) {
			loadFromSettings(coder.modelSettings);
			if (activeProviderId !== null) {
				if (coder.providerModels[activeProviderId] === undefined) {
					void coder.loadProviderModels(activeProviderId);
				}
			} else if (coder.routerModels === null) {
				void coder.loadModels();
			}
		} else if (coder.routerModels === null) {
			void coder.loadModels();
		}
		void coder.loadWebSearchConfigured();
	});

	// Commit the textbox values back to whichever provider's slot
	// is active right now, then flip `activeProviderId` and load
	// the destination's picks. Keeps the modal's "save once at the
	// end" semantics: no IPC happens until the user clicks Save.
	function switchActiveProvider(nextId: string | null): void {
		if (nextId === activeProviderId) {
			return;
		}
		// Snapshot current textboxes into the outgoing slot.
		if (activeProviderId === null) {
			// HF: textboxes map straight onto the local HF picks.
		} else {
			providers = providers.map((p) =>
				p.id === activeProviderId ? { ...p, standard_model: standardModel, cheap_model: cheapModel } : p,
			);
		}
		activeProviderId = nextId;
		if (nextId === null) {
			// HF: restore the HF picks the user typed earlier (or
			// from settings on initial load).
			const settings = coder.modelSettings;
			standardModel = settings?.standard_model ?? '';
			cheapModel = settings?.cheap_model ?? '';
			if (coder.routerModels === null) {
				void coder.loadModels();
			}
		} else {
			const entry = providers.find((p) => p.id === nextId);
			standardModel = entry?.standard_model ?? '';
			cheapModel = entry?.cheap_model ?? '';
			if (coder.providerModels[nextId] === undefined) {
				void coder.loadProviderModels(nextId);
			}
		}
		expandedModelId = null;
	}

	function openAddProvider(kind: ProviderKind): void {
		void (async () => {
			const id = await coder.newProviderId();
			const preset = presetForKind(kind);
			providerDraft = {
				id,
				label: preset?.label ?? '',
				kind,
				base_url: preset?.base_url ?? '',
				api_key: '',
				is_new: true,
			};
			probeMessage = null;
			probeError = null;
		})();
	}

	function openEditProvider(id: string): void {
		const entry = providers.find((p) => p.id === id);
		if (!entry) {
			return;
		}
		providerDraft = {
			id: entry.id,
			label: entry.label,
			kind: entry.kind,
			base_url: entry.base_url,
			api_key: '',
			is_new: false,
		};
		probeMessage = null;
		probeError = null;
	}

	function closeProviderDraft(): void {
		providerDraft = null;
		probeMessage = null;
		probeError = null;
		probing = false;
	}

	async function onProbeDraft(): Promise<void> {
		if (providerDraft === null) {
			return;
		}
		const baseUrl = providerDraft.base_url.trim();
		if (baseUrl.length === 0) {
			probeError = 'Enter a base URL first.';
			return;
		}
		probing = true;
		probeError = null;
		probeMessage = null;
		try {
			const result = await coder.probeProvider(baseUrl, providerDraft.api_key.trim(), providerDraft.kind);
			if (result.model_count > 0) {
				const sample = result.sample_model_ids.slice(0, 3).join(', ');
				probeMessage = `OK — ${result.model_count} model${result.model_count === 1 ? '' : 's'} reachable${sample ? ` (e.g. ${sample}).` : '.'}`;
			} else {
				probeMessage =
					'OK — endpoint reachable, but it does not expose `/v1/models`. You can still type a model id directly.';
			}
		} catch (err) {
			probeError = err instanceof Error ? err.message : String(err);
		} finally {
			probing = false;
		}
	}

	async function onSaveDraft(): Promise<void> {
		if (providerDraft === null) {
			return;
		}
		const draft = providerDraft;
		const label = draft.label.trim();
		const baseUrl = draft.base_url.trim();
		if (label.length === 0 || baseUrl.length === 0) {
			probeError = 'Label and base URL are required.';
			return;
		}
		const cleanedKey = draft.api_key.trim();
		try {
			// Persist the key first so the runner's `has_api_key`
			// flips to true at the moment the config lands.
			if (cleanedKey.length > 0) {
				await coder.setProviderApiKey(draft.id, cleanedKey);
			}
			const existing = providers.find((p) => p.id === draft.id);
			const next: CoderProviderConfig = {
				id: draft.id,
				label,
				kind: draft.kind,
				base_url: baseUrl,
				standard_model: existing?.standard_model ?? '',
				cheap_model: existing?.cheap_model ?? '',
				has_api_key: cleanedKey.length > 0 || (existing?.has_api_key ?? false),
			};
			await coder.saveProvider(next);
			// Re-sync local working copy from the refreshed
			// settings so the segmented control + `has_api_key`
			// flags are right.
			if (coder.modelSettings) {
				providers = cloneProviders(coder.modelSettings.providers);
			}
			coder.forgetProviderModels(draft.id);
			// Switch the active provider to the new entry so the
			// catalog tab below loads its `/v1/models` automatically
			// — for built-in presets the user still has to pick a
			// standard model from there before the outer Save
			// button enables.
			if (draft.is_new) {
				switchActiveProvider(draft.id);
			}
			closeProviderDraft();
		} catch (err) {
			probeError = err instanceof Error ? err.message : String(err);
		}
	}

	async function onClearDraftKey(): Promise<void> {
		if (providerDraft === null) {
			return;
		}
		try {
			await coder.clearProviderApiKey(providerDraft.id);
			if (coder.modelSettings) {
				providers = cloneProviders(coder.modelSettings.providers);
			}
			providerDraft = { ...providerDraft, api_key: '' };
			probeMessage = 'API key cleared.';
		} catch (err) {
			probeError = err instanceof Error ? err.message : String(err);
		}
	}

	async function onDeleteDraft(): Promise<void> {
		if (providerDraft === null || providerDraft.is_new) {
			return;
		}
		const id = providerDraft.id;
		try {
			await coder.deleteProvider(id);
			if (coder.modelSettings) {
				providers = cloneProviders(coder.modelSettings.providers);
			}
			if (activeProviderId === id) {
				switchActiveProvider(null);
			}
			closeProviderDraft();
		} catch (err) {
			probeError = err instanceof Error ? err.message : String(err);
		}
	}

	const activeProvider = $derived(
		activeProviderId === null ? null : (providers.find((p) => p.id === activeProviderId) ?? null),
	);
	const isHfActive = $derived(activeProviderId === null);
	// `/v1/models` rows for the active user provider, when any.
	// `null` = still loading; `[]` = no catalog (server doesn't
	// expose `/v1/models`, or we got an error and cached an empty
	// array so the spinner stops).
	const providerCatalog = $derived(activeProviderId === null ? null : (coder.providerModels[activeProviderId] ?? null));
	const filteredProviderCatalog = $derived.by(() => {
		const rows = providerCatalog ?? [];
		const needle = modelSearch.trim().toLowerCase();
		if (needle.length === 0) {
			return rows;
		}
		return rows.filter(
			(r) => r.id.toLowerCase().includes(needle) || (r.owned_by?.toLowerCase().includes(needle) ?? false),
		);
	});

	async function onSaveWebKey(): Promise<void> {
		const trimmed = webKeyDraft.trim();
		if (trimmed.length === 0) {
			webKeyError = 'Paste a Tavily API key first.';
			return;
		}
		webKeySaving = true;
		webKeyError = null;
		try {
			await coder.saveWebSearchKey(trimmed);
			webKeyDraft = '';
		} catch (err) {
			webKeyError = err instanceof Error ? err.message : String(err);
		} finally {
			webKeySaving = false;
		}
	}

	async function onClearWebKey(): Promise<void> {
		webKeySaving = true;
		webKeyError = null;
		try {
			await coder.clearWebSearchKey();
			webKeyDraft = '';
		} catch (err) {
			webKeyError = err instanceof Error ? err.message : String(err);
		} finally {
			webKeySaving = false;
		}
	}

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

	// Built-in providers (OpenRouter, Anthropic) require an
	// explicit standard-model pick from the catalog before Save
	// can fire — there's no sensible hardcoded default per
	// preset, and an empty slug would 404 on the first turn. The
	// HF route and free-form Custom providers stay loose: HF has
	// a backend-side default; Custom is intentionally hand-edit
	// territory.
	const requiresStandardPick = $derived(
		activeProvider !== null && (activeProvider.kind === 'open_router' || activeProvider.kind === 'anthropic'),
	);
	const standardModelSelected = $derived(standardModel.trim().length > 0);
	const saveBlockedBecauseNoStandard = $derived(requiresStandardPick && !standardModelSelected);

	async function onSave(): Promise<void> {
		saving = true;
		saveError = null;
		// Commit the textbox values back to whichever slot is
		// currently active before snapshotting the picker state.
		const standard = standardModel.trim();
		const cheap = cheapModel.trim();
		let hfStandard = coder.modelSettings?.standard_model ?? '';
		let hfCheap = coder.modelSettings?.cheap_model ?? '';
		let providersToSave = cloneProviders(providers);
		if (activeProviderId === null) {
			hfStandard = standard;
			hfCheap = cheap;
		} else {
			providersToSave = providersToSave.map((p) =>
				p.id === activeProviderId ? { ...p, standard_model: standard, cheap_model: cheap } : p,
			);
		}
		const next: CoderModelSettings = {
			standard_model: hfStandard,
			cheap_model: hfCheap,
			bill_to: billTo.trim(),
			active_provider: activeProviderId,
			providers: providersToSave,
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

		<!-- Provider switcher. HF is always implicit + first;
			 user-added providers sit alongside, and the `+ Add`
			 button opens the inline editor below. Switching here
			 commits the textbox picks to the outgoing slot and
			 loads the destination's picks — no IPC until Save. -->
		<section class="provider-switcher" aria-label="Active provider">
			<button
				type="button"
				class="provider-tab"
				class:active={activeProviderId === null}
				onclick={() => switchActiveProvider(null)}
			>
				Hugging Face
			</button>
			{#each providers as p (p.id)}
				<button
					type="button"
					class="provider-tab"
					class:active={activeProviderId === p.id}
					onclick={() => switchActiveProvider(p.id)}
					title={p.base_url}
				>
					{p.label}
					{#if !p.has_api_key && !isLocalUrl(p.base_url)}
						<span class="provider-tab-flag" title="No API key configured">no key</span>
					{/if}
				</button>
			{/each}
			<span class="provider-add-row">
				<span class="provider-add-label">+ Add</span>
				{#each BUILTIN_PRESETS as preset (preset.kind)}
					<button
						type="button"
						class="provider-tab add"
						onclick={() => openAddProvider(preset.kind)}
						title={preset.base_url}
					>
						{preset.label}
					</button>
				{/each}
				<button type="button" class="provider-tab add" onclick={() => openAddProvider('custom')}>Custom…</button>
			</span>
			{#if activeProvider !== null}
				<button type="button" class="provider-edit" onclick={() => openEditProvider(activeProvider.id)}>Edit</button>
			{/if}
		</section>

		{#if providerDraft !== null}
			{@const preset = presetForKind(providerDraft.kind)}
			<section class="provider-draft" aria-label="Provider details">
				<div class="draft-grid">
					<label class="field">
						<span class="label-row"><span class="label-name">Label</span></span>
						<input
							type="text"
							bind:value={providerDraft.label}
							placeholder={preset?.label ?? 'My provider'}
							spellcheck="false"
							autocomplete="off"
						/>
					</label>
					<label class="field">
						<span class="label-row">
							<span class="label-name">Base URL</span>
							{#if preset !== null}
								<span class="key-status configured">{preset.label} preset</span>
							{/if}
						</span>
						<input
							type="text"
							bind:value={providerDraft.base_url}
							placeholder={preset?.base_url ?? 'https://example.com/v1'}
							spellcheck="false"
							autocomplete="off"
							readonly={preset !== null}
						/>
						{#if preset !== null}
							<span class="hint">
								Built-in preset — URL is locked to the upstream API root. Pick <em>Custom…</em> instead if you need a different
								endpoint.
							</span>
						{/if}
					</label>
					<label class="field draft-key">
						<span class="label-row">
							<span class="label-name">API key</span>
							{#if !providerDraft.is_new}
								{@const existing = providers.find((p) => p.id === providerDraft?.id)}
								{#if existing?.has_api_key}
									<span class="key-status configured">key configured</span>
								{:else}
									<span class="key-status missing">no key</span>
								{/if}
							{/if}
						</span>
						<input
							type="password"
							bind:value={providerDraft.api_key}
							placeholder={providerDraft.is_new
								? (preset?.api_key_placeholder ?? 'sk-...')
								: 'Paste a new key to replace'}
							spellcheck="false"
							autocomplete="off"
						/>
						<span class="hint">
							{#if preset !== null}
								Get a key at <a href={preset.api_key_dashboard_url} target="_blank" rel="noreferrer noopener">
									{preset.api_key_dashboard_label}
								</a>.
							{/if}
							Stored in your OS keyring, never read back into this dialog. Leave blank for keyless local servers (<code
								>localhost</code
							>, <code>*.local</code>).
						</span>
					</label>
				</div>
				<div class="draft-actions">
					<button type="button" class="secondary" onclick={onProbeDraft} disabled={probing}>
						{probing ? 'Probing…' : 'Verify'}
					</button>
					{#if !providerDraft.is_new}
						<button type="button" class="secondary" onclick={onClearDraftKey}>Clear key</button>
						<button type="button" class="danger" onclick={onDeleteDraft}>Delete provider</button>
					{/if}
					<span class="flex-spacer"></span>
					<button type="button" class="secondary" onclick={closeProviderDraft}>Close</button>
					<button type="button" class="primary" onclick={onSaveDraft}>
						{providerDraft.is_new ? 'Save provider' : 'Save changes'}
					</button>
				</div>
				{#if probeError !== null}
					<p class="error">{probeError}</p>
				{:else if probeMessage !== null}
					<p class="probe-ok">{probeMessage}</p>
				{/if}
			</section>
		{/if}

		{#if providerDraft === null}
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

				{#if isHfActive}
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
							Sent as <code>X-HF-Bill-To</code>. Personal = your own HF account. Orgs you've authorized moon-ide for
							show up here; ones that can't pay are disabled. If an org you expect is missing, sign out and back in and
							tick it at the OAuth consent screen.
						</span>
					</label>
				{/if}

				<!-- Web-search subsection. Separate-but-inline rather than
				 a second modal because the team has one knob to set
				 here (a Tavily API key) and the discoverability win
				 from grouping it with the rest of the agent settings
				 outweighs the small layout overhead. The key itself
				 never round-trips back from the keyring; the UI just
				 knows whether one is set. -->
				<div class="web-key field">
					<span class="label-row">
						<span class="label-name">Web search (Tavily)</span>
						{#if coder.webSearchConfigured === true}
							<span class="key-status configured" title="Key stored in OS keyring">key configured</span>
						{:else if coder.webSearchConfigured === false}
							<span class="key-status missing">no key</span>
						{/if}
					</span>
					<div class="web-key-row">
						<input
							type="password"
							bind:value={webKeyDraft}
							placeholder={coder.webSearchConfigured ? 'Paste a new key to replace' : 'tvly-...'}
							spellcheck="false"
							autocomplete="off"
							disabled={webKeySaving}
						/>
						<button
							type="button"
							class="primary"
							onclick={onSaveWebKey}
							disabled={webKeySaving || webKeyDraft.trim().length === 0}
						>
							{coder.webSearchConfigured ? 'Replace' : 'Save'}
						</button>
						{#if coder.webSearchConfigured === true}
							<button type="button" class="secondary" onclick={onClearWebKey} disabled={webKeySaving}>Clear</button>
						{/if}
					</div>
					<span class="hint">
						Enables the <code>web_search</code> tool — Tavily for the SERP, Jina Reader for the page fetch (no second
						key needed). Get a free key at <code>tavily.com</code>; stored in your OS keyring, never read back into this
						dialog. Leave blank to disable web search entirely (the model won't see the tool).
					</span>
					{#if webKeyError !== null}
						<span class="error">{webKeyError}</span>
					{/if}
				</div>
			</section>

			<section class="catalog">
				<header class="catalog-header">
					<span class="catalog-title">
						Catalog ({editingTier} model)
					</span>
					<input
						type="search"
						bind:value={modelSearch}
						placeholder={isHfActive ? 'Filter by name, owner, or provider…' : 'Filter by model id or owner…'}
						spellcheck="false"
						autocomplete="off"
					/>
					{#if !isHfActive && activeProviderId !== null}
						{@const id = activeProviderId}
						<button
							type="button"
							class="catalog-refresh"
							title="Re-fetch /v1/models from this provider"
							onclick={() => {
								coder.forgetProviderModels(id);
								void coder.loadProviderModels(id);
							}}
						>
							Refresh
						</button>
					{/if}
				</header>
				{#if !isHfActive}
					<!-- User-added provider: render the flat /v1/models
					 list. The picker writes the slug verbatim into
					 the textbox above; there's no `:provider`
					 suffix because non-HF routes don't multiplex. -->
					{#if coder.modelsLoading && providerCatalog === null}
						<p class="catalog-hint">Loading models from this provider…</p>
					{:else if providerCatalog === null}
						<p class="catalog-hint">Open the provider's catalog…</p>
						<button
							type="button"
							class="secondary"
							onclick={() => activeProviderId !== null && coder.loadProviderModels(activeProviderId)}
						>
							Load catalog
						</button>
					{:else if providerCatalog.length === 0 && coder.modelsError !== null}
						<p class="error">{coder.modelsError}</p>
						<p class="catalog-hint">
							You can still type a model id directly into the {editingTier} model field above.
						</p>
					{:else if filteredProviderCatalog.length === 0}
						<p class="catalog-hint">No models match this filter.</p>
					{:else}
						<ul class="flat-catalog">
							{#each filteredProviderCatalog as row (row.id)}
								{@const picked = (editingTier === 'standard' ? standardModel : cheapModel) === row.id}
								<li>
									<button
										type="button"
										class="flat-row"
										class:picked
										onclick={() => {
											if (editingTier === 'standard') {
												standardModel = row.id;
											} else {
												cheapModel = row.id;
											}
										}}
									>
										<div class="flat-row-main">
											<span class="flat-id">{row.id}</span>
											{#if row.name && row.name !== row.id}
												<span class="flat-name">{row.name}</span>
											{/if}
											{#if picked}
												<span class="flat-picked">Picked</span>
											{/if}
										</div>
										<div class="flat-row-meta">
											{#if row.context_length}
												<span>{formatContext(row.context_length)} ctx</span>
											{/if}
											{#if row.pricing_in_per_million !== null && row.pricing_in_per_million !== undefined}
												<span>
													${parseFloat(row.pricing_in_per_million.toFixed(3))}/${parseFloat(
														(row.pricing_out_per_million ?? row.pricing_in_per_million).toFixed(3),
													)} per M
												</span>
											{/if}
											{#if row.owned_by}
												<span class="flat-owner">{row.owned_by}</span>
											{/if}
										</div>
									</button>
								</li>
							{/each}
						</ul>
					{/if}
				{:else if coder.modelsLoading && coder.routerModels === null}
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
								<button
									type="button"
									class="model-row"
									aria-expanded={expanded}
									onclick={() => toggleExpanded(model.id)}
								>
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
		{/if}

		<footer>
			{#if saveError}
				<p class="error">{saveError}</p>
			{/if}
			{#if saveBlockedBecauseNoStandard}
				<p class="hint footer-hint">
					Pick a standard model from the catalog below to enable Save. Cheap model is optional — it falls back to the
					standard slug when blank.
				</p>
			{/if}
			<div class="footer-actions">
				<button type="button" class="secondary" onclick={onCancel} disabled={saving}>Cancel</button>
				<button type="button" class="primary" onclick={onSave} disabled={saving || saveBlockedBecauseNoStandard}>
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
	.web-key-row {
		display: flex;
		gap: 6px;
		align-items: stretch;
	}
	.web-key-row input {
		flex: 1 1 auto;
		min-width: 0;
	}
	.web-key-row button {
		flex: 0 0 auto;
		padding: 0 12px;
		font-size: 11px;
	}
	.key-status {
		font-size: 10px;
		text-transform: uppercase;
		letter-spacing: 0.06em;
		padding: 1px 6px;
		border-radius: 3px;
		border: 1px solid var(--m-border);
	}
	.key-status.configured {
		color: var(--m-success, #38a169);
		border-color: var(--m-success, #38a169);
	}
	.key-status.missing {
		color: var(--m-fg-muted);
	}
	.provider-switcher {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 6px;
		padding-bottom: 10px;
		border-bottom: 1px solid var(--m-border);
	}
	.provider-tab {
		background: transparent;
		border: 1px solid var(--m-border);
		border-radius: 4px;
		color: var(--m-fg);
		font-size: 12px;
		padding: 4px 10px;
		cursor: pointer;
		display: inline-flex;
		align-items: center;
		gap: 6px;
	}
	.provider-tab.active {
		background: var(--m-accent);
		border-color: var(--m-accent);
		color: var(--m-on-accent, #fff);
	}
	.provider-tab.add {
		border-style: dashed;
		color: var(--m-fg-muted);
	}
	.provider-add-row {
		display: inline-flex;
		align-items: center;
		gap: 4px;
		padding-left: 4px;
		border-left: 1px solid var(--m-border, transparent);
		margin-left: 4px;
	}
	.provider-add-label {
		font-size: 10px;
		text-transform: uppercase;
		letter-spacing: 0.04em;
		color: var(--m-fg-muted);
		padding-right: 2px;
	}
	.footer-hint {
		font-size: 11px;
		color: var(--m-fg-muted);
		margin: 0;
	}
	.provider-tab-flag {
		font-size: 9px;
		text-transform: uppercase;
		letter-spacing: 0.05em;
		padding: 1px 5px;
		border-radius: 3px;
		border: 1px solid currentColor;
		color: var(--m-fg-muted);
	}
	.provider-tab.active .provider-tab-flag {
		color: var(--m-on-accent, #fff);
	}
	.provider-edit {
		background: transparent;
		border: 1px solid var(--m-border);
		color: var(--m-fg-muted);
		border-radius: 4px;
		font-size: 11px;
		padding: 3px 8px;
		cursor: pointer;
		margin-left: auto;
	}
	.provider-draft {
		display: flex;
		flex-direction: column;
		gap: 10px;
		padding: 12px;
		background: var(--m-bg-overlay);
		border: 1px solid var(--m-border);
		border-radius: 6px;
	}
	.draft-grid {
		display: grid;
		grid-template-columns: minmax(160px, 1fr) minmax(220px, 2fr);
		gap: 10px 14px;
	}
	.draft-key {
		grid-column: 1 / -1;
	}
	.draft-actions {
		display: flex;
		gap: 8px;
		align-items: center;
		flex-wrap: wrap;
	}
	.flex-spacer {
		flex: 1 1 auto;
	}
	.danger {
		background: transparent;
		border: 1px solid var(--m-error, #d34c4c);
		color: var(--m-error, #d34c4c);
		border-radius: 4px;
		padding: 6px 14px;
		font-size: 12px;
		cursor: pointer;
	}
	.probe-ok {
		margin: 0;
		font-size: 11px;
		color: var(--m-success, #38a169);
	}
	.flat-catalog {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: 2px;
		max-height: 320px;
		overflow-y: auto;
	}
	.flat-row {
		display: flex;
		flex-direction: column;
		align-items: stretch;
		gap: 2px;
		width: 100%;
		text-align: left;
		background: transparent;
		border: 1px solid transparent;
		border-radius: 4px;
		color: var(--m-fg);
		padding: 6px 10px;
		font-size: 12px;
		font-family: var(--m-font-mono, ui-monospace, monospace);
		cursor: pointer;
	}
	.flat-row:hover {
		background: var(--m-bg-overlay);
	}
	.flat-row.picked {
		border-color: var(--m-accent);
		background: color-mix(in srgb, var(--m-accent) 12%, transparent);
	}
	.flat-row-main {
		display: flex;
		align-items: baseline;
		gap: 10px;
	}
	.flat-row-meta {
		display: flex;
		align-items: baseline;
		gap: 10px;
		font-family: var(--m-font, system-ui, sans-serif);
		font-size: 10px;
		color: var(--m-fg-muted);
	}
	.flat-row-meta:empty {
		display: none;
	}
	.flat-id {
		flex: 0 0 auto;
	}
	.flat-name {
		flex: 1 1 auto;
		font-family: var(--m-font, system-ui, sans-serif);
		font-size: 11px;
		color: var(--m-fg-muted);
	}
	.flat-owner {
		font-size: 10px;
		color: var(--m-fg-muted);
		font-family: var(--m-font, system-ui, sans-serif);
	}
	.flat-picked {
		font-size: 10px;
		text-transform: uppercase;
		letter-spacing: 0.05em;
		color: var(--m-accent);
		font-family: var(--m-font, system-ui, sans-serif);
	}
	.catalog-refresh {
		background: transparent;
		border: 1px solid var(--m-border);
		color: var(--m-fg-muted);
		border-radius: 4px;
		font-size: 11px;
		padding: 3px 8px;
		cursor: pointer;
	}
</style>
