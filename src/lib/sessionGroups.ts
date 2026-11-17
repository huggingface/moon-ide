// Pure sessions-list logic: the one state a row shows, and the section
// that state files it under. Kept apart from the coder store so it can
// be tested without runes.

/** What the store and the summary know about one session. */
export type SessionRowSignals = {
	awaitingInput: boolean;
	running: boolean;
	/** Finished while the user was looking elsewhere. */
	attention: boolean;
	lastError: boolean;
	interrupted: boolean;
	/** The user set the session's last outcome aside. */
	dismissed: boolean;
};

export type SessionRowState = 'awaiting' | 'running' | 'failed' | 'interrupted' | 'finished' | 'idle';

/** Several signals can hold at once (a failed turn also raises
 *  attention); a live turn outranks how the previous one ended, and a
 *  bad ending outranks "finished". A dismissal covers how the last
 *  turn ended, never a live one: an agent waiting on an answer is
 *  blocked whether or not the user wants to hear about it. */
export function sessionRowState(signals: SessionRowSignals): SessionRowState {
	if (signals.awaitingInput) {
		return 'awaiting';
	}
	if (signals.running) {
		return 'running';
	}
	if (signals.dismissed) {
		return 'idle';
	}
	if (signals.lastError) {
		return 'failed';
	}
	if (signals.interrupted) {
		return 'interrupted';
	}
	if (signals.attention) {
		return 'finished';
	}
	return 'idle';
}

export type SessionGroupId = 'needs-you' | 'running' | 'earlier';

export type SessionGroup<T> = {
	id: SessionGroupId;
	title: string;
	items: T[];
};

const GROUP_TITLES: Record<SessionGroupId, string> = {
	'needs-you': 'Needs you',
	running: 'Running',
	earlier: 'Earlier',
};

/** Display order: what is blocked on the user first. */
const GROUP_ORDER: SessionGroupId[] = ['needs-you', 'running', 'earlier'];

export function sessionGroupOf(state: SessionRowState): SessionGroupId {
	if (state === 'running') {
		return 'running';
	}
	return state === 'idle' ? 'earlier' : 'needs-you';
}

/** Split `sessions` into the non-empty groups, in display order.
 *  Items keep their incoming relative order inside each group. */
export function groupSessions<T>(sessions: readonly T[], stateOf: (session: T) => SessionRowState): SessionGroup<T>[] {
	const buckets: Record<SessionGroupId, T[]> = { 'needs-you': [], running: [], earlier: [] };
	for (const session of sessions) {
		buckets[sessionGroupOf(stateOf(session))].push(session);
	}
	return GROUP_ORDER.filter((id) => buckets[id].length > 0).map((id) => ({
		id,
		title: GROUP_TITLES[id],
		items: buckets[id],
	}));
}

/** States a row can be dismissed from. */
export function isDismissable(state: SessionRowState): boolean {
	return state === 'failed' || state === 'interrupted' || state === 'finished';
}

export type SessionDismissal = { id: string; updated_at_ms: number };

/** Dismissals kept at most; the oldest fall off. Sessions outlive
 *  their dismissal harmlessly (they are idle by then or resurface). */
const MAX_DISMISSALS = 500;

/** A dismissal holds until the session is written to again. */
export function isDismissed(
	dismissals: readonly SessionDismissal[],
	session: { id: string; updated_at_ms: number },
): boolean {
	return dismissals.some((entry) => entry.id === session.id && entry.updated_at_ms >= session.updated_at_ms);
}

export function withDismissal(dismissals: readonly SessionDismissal[], session: SessionDismissal): SessionDismissal[] {
	return [
		...dismissals.filter((entry) => entry.id !== session.id),
		{ id: session.id, updated_at_ms: session.updated_at_ms },
	].slice(-MAX_DISMISSALS);
}
