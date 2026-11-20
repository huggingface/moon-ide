// What the agent has said about lines of a pull request's diff, and
// what the user asked back (ADR 0124). Kept per head commit: a note
// is about a line of that exact version. Local to the IDE; nothing
// here is ever sent to GitHub.
//
// Threads are replaced, never mutated, and an untouched thread keeps
// its identity: the diff's cards are redrawn only when theirs changed,
// so a half-typed reply in one survives a note landing in another.

import type { ReviewSide } from './protocol';

export type NoteEntry = { from: 'agent' | 'you'; text: string };

export type NoteThread = {
	path: string;
	side: ReviewSide;
	line: number;
	entries: readonly NoteEntry[];
	/** The agent has been asked about this line and hasn't answered. */
	pending: boolean;
};

export type NotePlace = { path: string; side: ReviewSide; line: number };

/** What a file's diff shows of the notes and how it asks for more. */
export type FileNotes = {
	threads: readonly NoteThread[];
	/** Explain a block of the new version, or the whole file's change. */
	onExplain: (lines: { from: number; to: number } | null) => void;
	onReply: (thread: NoteThread, text: string) => void;
	onDismiss: (thread: NoteThread) => void;
};

function at(thread: NoteThread, place: NotePlace): boolean {
	return thread.path === place.path && thread.side === place.side && thread.line === place.line;
}

function withPending(thread: NoteThread, pending: boolean): NoteThread {
	return { path: thread.path, side: thread.side, line: thread.line, entries: thread.entries, pending };
}

function upsert(
	threads: readonly NoteThread[],
	place: NotePlace,
	change: (thread: NoteThread) => NoteThread,
): readonly NoteThread[] {
	if (!threads.some((thread) => at(thread, place))) {
		return [...threads, change({ ...place, entries: [], pending: false })];
	}
	return threads.map((thread) => (at(thread, place) ? change(thread) : thread));
}

/** The agent's note lands: on the thread of its line, or a new one. */
export function addNote(threads: readonly NoteThread[], place: NotePlace, text: string): readonly NoteThread[] {
	return upsert(threads, place, (thread) => ({
		...thread,
		entries: [...thread.entries, { from: 'agent', text }],
		pending: false,
	}));
}

/** The user asks about a line: `question` is their reply on an
 *  existing thread, or `null` for the first "explain this". */
export function askAt(
	threads: readonly NoteThread[],
	place: NotePlace,
	question: string | null,
): readonly NoteThread[] {
	return upsert(threads, place, (thread) => ({
		...thread,
		entries: question === null ? thread.entries : [...thread.entries, { from: 'you', text: question }],
		pending: true,
	}));
}

/** The agent's turn is over: nothing is pending any more, and a line
 *  it was asked about but said nothing on loses its empty card. */
export function settle(threads: readonly NoteThread[]): readonly NoteThread[] {
	if (!threads.some((thread) => thread.pending)) {
		return threads;
	}
	return threads
		.filter((thread) => !thread.pending || thread.entries.length > 0)
		.map((thread) => (thread.pending ? withPending(thread, false) : thread));
}

export function dismiss(threads: readonly NoteThread[], place: NotePlace): readonly NoteThread[] {
	return threads.filter((thread) => !at(thread, place));
}

/** What the agent is told when the user clicks "Explain" on a block
 *  (`lines`) or on a whole file (`null`). */
export function explainPrompt(
	pr: { number: number; title: string },
	path: string,
	lines: { from: number; to: number } | null,
): string {
	const where =
		lines === null ? `\`${path}\`` : `\`${path}\` lines ${lines.from}-${lines.to} (line numbers of the new version)`;
	const anchor = lines === null ? 'the first changed line' : `line ${lines.from}`;
	return (
		`I'm reviewing pull request #${pr.number} (${pr.title}). Explain the change in ${where}: what it does and why, ` +
		`and anything a reviewer should check. Look at whatever else you need to be sure. ` +
		`Put the answer on the diff with \`review_note\` (path \`${path}\`, ${anchor}), in a few sentences.`
	);
}

/** What the agent is told when the user replies on a note's card. */
export function replyPrompt(thread: NoteThread, question: string): string {
	return (
		`About your note on \`${thread.path}\` line ${thread.line}: ${question}\n\n` +
		`Answer with \`review_note\` on the same path and line.`
	);
}

/** What a question asked from the pull request's tab is about, beyond
 *  the pull request itself. */
export type TalkAbout =
	| { kind: 'comment'; author: string | null; body: string; url: string }
	| { kind: 'thread'; path: string; line: number | null; author: string | null; body: string };

export type TalkEntry = { from: 'you' | 'agent'; text: string; about: string | null };

/** A question asked from the tab, and what came back. */
export type Talk = { entries: readonly TalkEntry[]; waiting: boolean };

const QUOTE_MAX = 2_000;

/** How `about` reads on the user's own card. */
export function aboutLabel(about: TalkAbout): string {
	const who = about.author ?? 'someone';
	return about.kind === 'comment'
		? `About ${who}'s comment`
		: `About ${who}'s thread on ${about.path}${about.line === null ? '' : `:${about.line}`}`;
}

function quote(text: string): string {
	const cut = text.length > QUOTE_MAX ? `${text.slice(0, QUOTE_MAX)}…` : text;
	return cut
		.trim()
		.split('\n')
		.map((line) => `> ${line}`)
		.join('\n');
}

/** What the agent is told when the user asks from the tab. The answer
 *  is the turn's reply, shown in the tab. */
export function talkPrompt(pr: { number: number; title: string }, question: string, about: TalkAbout | null): string {
	let context = '';
	if (about?.kind === 'comment') {
		context = `\n\nIt is about this comment by ${about.author ?? 'someone'} (${about.url}):\n\n${quote(about.body)}`;
	} else if (about?.kind === 'thread') {
		const where = `\`${about.path}\`${about.line === null ? '' : ` line ${about.line}`}`;
		context = `\n\nIt is about this review thread on ${where}, started by ${about.author ?? 'someone'}:\n\n${quote(about.body)}`;
	}
	return (
		`I'm reviewing pull request #${pr.number} (${pr.title}) and ask you, from its review tab: ${question.trim()}${context}\n\n` +
		`Answer in your reply, briefly: it is shown in the tab. Look at whatever you need to be sure. ` +
		`Use \`review_note\` as well only when pointing at a line of the diff helps.`
	);
}
