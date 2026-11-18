// The agent's notes on the diffs being reviewed (ADR 0124), as they
// arrive from `review_note`, and which session is each pull request
// tab's agent. In memory only.

import type { CoderEventEnvelope } from './protocol';
import { addNote, askAt, dismiss, settle, type NotePlace, type NoteThread } from './reviewNotes';
import { subscribe } from './transport';

const NONE: readonly NoteThread[] = [];

class ReviewNotesStore {
	/** Checkout path → the notes on its diff. A note is filed under
	 *  the checkout its agent works in. */
	byRoot = $state.raw<Record<string, readonly NoteThread[]>>({});
	/** Pull request tab → the session it asks, and that session's
	 *  checkout. */
	agents = $state<Record<string, { sessionId: string; root: string }>>({});
	#rootBySession = new Map<string, string>();

	threadsFor(root: string | null): readonly NoteThread[] {
		return root === null ? NONE : (this.byRoot[root] ?? NONE);
	}

	#update(root: string, change: (threads: readonly NoteThread[]) => readonly NoteThread[]) {
		const before = this.byRoot[root] ?? NONE;
		const after = change(before);
		if (after !== before) {
			this.byRoot = { ...this.byRoot, [root]: after };
		}
	}

	registerAgent(tab: string, sessionId: string, root: string) {
		this.agents[tab] = { sessionId, root };
		this.#rootBySession.set(sessionId, root);
	}

	forgetAgent(tab: string) {
		delete this.agents[tab];
	}

	/** The user asked the agent about `place`; its card waits. */
	ask(root: string, place: NotePlace, question: string | null) {
		this.#update(root, (threads) => askAt(threads, place, question));
	}

	dismiss(root: string, place: NotePlace) {
		this.#update(root, (threads) => dismiss(threads, place));
	}

	/** Nothing is waiting on `root` any more (a request that failed). */
	settle(root: string) {
		this.#update(root, settle);
	}

	clear(root: string) {
		this.#update(root, () => NONE);
	}

	#onEvent(envelope: CoderEventEnvelope) {
		const event = envelope.event.kind === 'subagent_event' ? envelope.event.inner : envelope.event;
		if (event.kind === 'review_note') {
			const { root, path, side, line, text } = event;
			this.#update(root, (threads) => addNote(threads, { path, side, line }, text));
			return;
		}
		const ended = envelope.event.kind;
		if (ended !== 'turn_complete' && ended !== 'aborted' && ended !== 'error') {
			return;
		}
		const root = this.#rootBySession.get(envelope.session_id);
		if (root !== undefined) {
			this.settle(root);
		}
	}

	#wired = false;

	async wireRuntime(): Promise<void> {
		if (this.#wired) {
			return;
		}
		this.#wired = true;
		try {
			await subscribe('coder:event', (payload: CoderEventEnvelope) => this.#onEvent(payload));
		} catch {
			// No event bus: notes won't arrive. The coder's own listener
			// reports the same failure.
		}
	}
}

export const reviewNotes = new ReviewNotesStore();
