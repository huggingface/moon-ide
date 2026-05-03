//! Reactive state for the IDE's bottom panel.
//!
//! The bottom panel is a horizontal strip below the editor area
//! that hosts long-lived auxiliary surfaces — service-log streams
//! today, terminals (Phase 5) and other diagnostics later. Each
//! surface is a tab; the panel itself just owns the chrome
//! (visibility, height, tab strip, active tab) and lets each tab
//! kind render its own body.
//!
//! Why a separate store
//! --------------------
//!
//! Folding this into `WorkspaceState` would couple region chrome
//! to per-folder UI state in a way that complicates persistence
//! and hot-reload. The panel's lifecycle is workspace-independent
//! — opening a different folder shouldn't drop your open log
//! tabs, and the panel's visibility persists across launches via
//! `app_state.json`.
//!
//! Tab kinds
//! ---------
//!
//! Today only a `placeholder` kind exists: the next slice
//! (service logs) plugs in `log` tabs. New kinds add a variant
//! to [`BottomPanelTab`] and a renderer branch in
//! `BottomPanel.svelte`.
//!
//! Persistence
//! -----------
//!
//! Visibility and height ride along with the rest of `AppState`
//! through `WorkspaceState.persistAppState`. Tab contents are
//! deliberately not persisted — they're backed by running
//! processes (`docker compose logs -f`, …) that don't survive a
//! launch, and silently re-spawning them at startup would
//! surprise the user.

import type { BottomPanelAppState, TerminalTarget } from './protocol';

/** Minimum panel height (px). Below this the tab strip and a
 * single line of body content stop being legible. */
const MIN_HEIGHT = 120;

/** Maximum panel height (px). Beyond this the editor area
 * shrinks below ~120px of content, which makes the IDE useless. */
const MAX_HEIGHT = 800;

/** Default panel height (px). Mirrored in the Rust default —
 * keep both in sync if you change one. */
export const DEFAULT_BOTTOM_PANEL_HEIGHT = 240;

/** Tabs the bottom panel can host.
 *
 * The discriminated `kind` field lets the renderer pick a body
 * component without runtime introspection. Tabs are intentionally
 * lean shells — kind-specific content (log line buffers, future
 * terminal session handles) lives in a sibling store keyed on
 * `id` so adding new kinds doesn't bloat this type. */
export type BottomPanelTab = PlaceholderTab | LogTab | TerminalTab;

export type PlaceholderTab = {
	id: string;
	title: string;
	kind: 'placeholder';
};

export type LogTab = {
	id: string;
	title: string;
	kind: 'log';
	/** Absolute path of the bound folder this stream belongs to. */
	folderPath: string;
	/** Compose service name being tailed. */
	service: string;
};

/** Terminal session tab. The `target` is captured at open time
 * and immutable for the tab's life — see ADR 0009. The store
 * (`terminal.svelte.ts`) holds the live xterm instance keyed
 * on `id`. */
export type TerminalTab = {
	id: string;
	title: string;
	kind: 'terminal';
	target: TerminalTarget;
};

class BottomPanelStore {
	#visible = $state(false);
	#height = $state(DEFAULT_BOTTOM_PANEL_HEIGHT);
	#tabs = $state<BottomPanelTab[]>([]);
	#activeId = $state<string | null>(null);

	/** Bound by `WorkspaceState.restoreAppState` so changes here
	 * trigger an `app_state_save`. Stays unset during construction
	 * to avoid persisting before hydration finishes. */
	#onChange: (() => void) | null = null;

	get visible(): boolean {
		return this.#visible;
	}

	get height(): number {
		return this.#height;
	}

	get tabs(): readonly BottomPanelTab[] {
		return this.#tabs;
	}

	get activeId(): string | null {
		return this.#activeId;
	}

	get activeTab(): BottomPanelTab | null {
		const id = this.#activeId;
		if (id === null) {
			return null;
		}
		return this.#tabs.find((t) => t.id === id) ?? null;
	}

	bindOnChange(handler: () => void): void {
		this.#onChange = handler;
	}

	hydrate(state: BottomPanelAppState): void {
		this.#visible = state.visible;
		this.#height = clampHeight(state.height);
	}

	serialise(): BottomPanelAppState {
		return { visible: this.#visible, height: this.#height };
	}

	toggle(): void {
		this.#visible = !this.#visible;
		this.#notify();
	}

	show(): void {
		if (this.#visible) {
			return;
		}
		this.#visible = true;
		this.#notify();
	}

	hide(): void {
		if (!this.#visible) {
			return;
		}
		this.#visible = false;
		this.#notify();
	}

	setHeight(px: number): void {
		const next = clampHeight(px);
		if (next === this.#height) {
			return;
		}
		this.#height = next;
		this.#notify();
	}

	setActive(id: string): void {
		if (this.#tabs.some((t) => t.id === id)) {
			this.#activeId = id;
		}
	}

	/** Append `tab` to the strip and make it active. Caller is
	 * responsible for picking a unique id (typically a stream
	 * UUID for log tabs). Existing tabs with the same id are
	 * left alone — use [`findLogTab`] before opening to avoid
	 * duplicates. */
	addTab(tab: BottomPanelTab): void {
		if (this.#tabs.some((t) => t.id === tab.id)) {
			this.#activeId = tab.id;
			return;
		}
		this.#tabs = [...this.#tabs, tab];
		this.#activeId = tab.id;
	}

	/** Find an existing log tab for `(folderPath, service)`, or
	 * `null` if none. Used by the Logs button to focus the
	 * existing stream rather than spawning a duplicate. */
	findLogTab(folderPath: string, service: string): LogTab | null {
		const tab = this.#tabs.find((t) => t.kind === 'log' && t.folderPath === folderPath && t.service === service);
		return tab && tab.kind === 'log' ? tab : null;
	}

	closeTab(id: string): void {
		const idx = this.#tabs.findIndex((t) => t.id === id);
		if (idx === -1) {
			return;
		}
		this.#tabs = this.#tabs.toSpliced(idx, 1);
		if (this.#activeId !== id) {
			return;
		}
		// Pick a sensible neighbour: prefer the tab that was to the
		// right of the closed one (matches editor tab close UX),
		// fall back to the previous one, or null when the strip is
		// empty.
		const next = this.#tabs[idx] ?? this.#tabs[idx - 1] ?? null;
		this.#activeId = next ? next.id : null;
	}

	#notify(): void {
		this.#onChange?.();
	}
}

function clampHeight(px: number): number {
	if (!Number.isFinite(px)) {
		return DEFAULT_BOTTOM_PANEL_HEIGHT;
	}
	if (px < MIN_HEIGHT) {
		return MIN_HEIGHT;
	}
	if (px > MAX_HEIGHT) {
		return MAX_HEIGHT;
	}
	return Math.round(px);
}

export const bottomPanel = new BottomPanelStore();
