//! Coder sessions the user set aside from the sessions list's "Needs
//! you" group without deleting them (ADR 0114). Local to the
//! workspace: nothing about the session itself changes, so its
//! failure or interruption is still there when it is opened.

import type { DismissedCoderSession } from './protocol';
import { isDismissed, withDismissal } from './sessionGroups';

class SessionDismissals {
	entries = $state<DismissedCoderSession[]>([]);

	#onChange: (() => void) | null = null;

	hydrate(entries: readonly DismissedCoderSession[]): void {
		this.entries = [...entries];
	}

	serialise(): DismissedCoderSession[] {
		return this.entries;
	}

	bindOnChange(handler: () => void): void {
		this.#onChange = handler;
	}

	isDismissed(session: { id: string; updated_at_ms: number }): boolean {
		return isDismissed(this.entries, session);
	}

	dismiss(sessions: readonly { id: string; updated_at_ms: number }[]): void {
		if (sessions.length === 0) {
			return;
		}
		this.entries = sessions.reduce(withDismissal, this.entries);
		this.#onChange?.();
	}

	forget(id: string): void {
		const next = this.entries.filter((entry) => entry.id !== id);
		if (next.length === this.entries.length) {
			return;
		}
		this.entries = next;
		this.#onChange?.();
	}
}

export const sessionDismissals = new SessionDismissals();
