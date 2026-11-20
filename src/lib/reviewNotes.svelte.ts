// The agent's notes on the diffs being reviewed (ADR 0124), as they
// arrive from `review_note`, which session is each pull request tab's
// agent, and what was asked of it from the tab and answered. In
// memory only.

import type { CoderEventEnvelope } from './protocol';
import {
	addNote,
	askAt,
	dismiss,
	settle,
	type NotePlace,
	type NoteThread,
	type Talk,
	type TalkEntry,
} from './reviewNotes';
import { subscribe } from './transport';

const NONE: readonly NoteThread[] = [];
const QUIET: Talk = { entries: [], waiting: false };

class ReviewNotesStore {
	/** Checkout path → the notes on its diff. A note is filed under
	 *  the checkout its agent works in. */
	byRoot = $state.raw<Record<string, readonly NoteThread[]>>({});
	/** Pull request tab → the session it asks, and that session's
	 *  checkout. */
	agents = $state<Record<string, { sessionId: string; root: string }>>({});
	#rootBySession = new Map<string, string>();
	/** Pull request tab → the questions asked from it. */
	talks = $state.raw<Record<string, Talk>>({});
	/** Session → the tab whose question its current turn answers. */
	#answering = new Map<string, string>();

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

	talkFor(tab: string | null): Talk {
		return tab === null ? QUIET : (this.talks[tab] ?? QUIET);
	}

	#setTalk(tab: string, talk: Talk) {
		this.talks = { ...this.talks, [tab]: talk };
	}

	/** The user asked from `tab`; the reply of the session's turn is
	 *  its answer. */
	askFromTab(tab: string, sessionId: string, question: string, about: string | null) {
		const talk = this.talkFor(tab);
		this.#setTalk(tab, { entries: [...talk.entries, { from: 'you', text: question, about }], waiting: true });
		this.#answering.set(sessionId, tab);
	}

	/** The question could not be sent. */
	failTalk(tab: string, sessionId: string, reason: string) {
		this.#answering.delete(sessionId);
		const talk = this.talkFor(tab);
		this.#setTalk(tab, { entries: [...talk.entries, { from: 'agent', text: reason, about: null }], waiting: false });
	}

	/** The reply so far: a turn says something before each round of
	 *  tools, and only its last word is the answer. */
	#answer(tab: string, text: string) {
		const talk = this.talkFor(tab);
		const entries = [...talk.entries];
		const last = entries.at(-1);
		const entry: TalkEntry = { from: 'agent', text, about: null };
		if (last?.from === 'agent' && talk.waiting) {
			entries[entries.length - 1] = entry;
		} else {
			entries.push(entry);
		}
		this.#setTalk(tab, { entries, waiting: talk.waiting });
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
		const tab = this.#answering.get(envelope.session_id);
		if (tab !== undefined && envelope.event.kind === 'assistant_message_end' && envelope.event.text.trim() !== '') {
			this.#answer(tab, envelope.event.text);
		}
		const ended = envelope.event.kind;
		if (ended !== 'turn_complete' && ended !== 'aborted' && ended !== 'error') {
			return;
		}
		if (tab !== undefined) {
			this.#answering.delete(envelope.session_id);
			if (envelope.event.kind === 'error') {
				this.#answer(tab, `The agent stopped: ${envelope.event.message}`);
			}
			this.#setTalk(tab, { entries: this.talkFor(tab).entries, waiting: false });
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
