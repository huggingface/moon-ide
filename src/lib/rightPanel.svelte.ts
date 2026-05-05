//! Reactive state for the right-side panel slot.
//!
//! The IDE has exactly one right-side panel slot; the chat and coder
//! surfaces are mutually exclusive tenants of it. Opening one swaps
//! the other out rather than stacking them — two simultaneous
//! right-side panels burn horizontal real estate that the editor
//! pane needs more, and the second column ends up squeezed into a
//! useless ~200 px strip on standard laptop screens.
//!
//! Persistence:
//! - Backed by `AppState.right_panel` (`'chat' | 'coder' | null`).
//! - Hydrated once on startup from `state.svelte.ts:restoreAppState`.
//! - Every flip writes through the dedicated `ui_set_right_panel`
//!   Tauri command. We don't piggyback on the existing
//!   `app_state_save` debounce because the slack poller's
//!   `panel_visible` input is gated server-side on this exact value
//!   and a 250 ms persist delay would burn API budget polling for a
//!   panel the user just closed.
//!
//! Width persistence is intentionally *not* here — that's per-panel
//! cosmetic state owned by `App.svelte` (each surface remembers its
//! own width). Add it if a user actually asks; until then there's
//! nothing meaningful to save.

import { ipc } from './ipc';
import type { RightPanelKind } from './protocol';

class RightPanelState {
	/** Which surface is mounted in the right slot, or `null` for closed. */
	kind = $state<RightPanelKind | null>(null);

	/** Apply the persisted pick at startup. Idempotent. */
	hydrate(kind: RightPanelKind | null): void {
		this.kind = kind;
	}

	/** Toggle the slot. If the requested surface is already mounted,
	 *  close the slot. Otherwise mount the requested surface (which
	 *  swaps the other one out, if any). */
	toggle(kind: RightPanelKind): void {
		if (this.kind === kind) {
			this.set(null);
			return;
		}
		this.set(kind);
	}

	/** Force a specific kind (or close the slot). Useful from
	 *  command-palette entries that want to *open* a surface
	 *  unconditionally rather than toggle it. */
	set(kind: RightPanelKind | null): void {
		if (this.kind === kind) {
			return;
		}
		this.kind = kind;
		// Fire-and-forget: a failed write only means the panel forgets
		// its state on the next launch. Loud toasts on every toggle
		// would be more annoying than the bug they catch.
		void ipc.ui.setRightPanel(kind).catch(() => {});
	}
}

export const rightPanel = new RightPanelState();
