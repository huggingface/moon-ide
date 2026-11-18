// Read-only pull-request annotations in a review section's diff
// (ADR 0113): existing review threads, and the nit-level notes moonbot
// keeps in the PR description, each shown under the line it is about.
//
// Separate from `reviewComments.ts` on purpose: those are the user's
// own editable drafts, anchored by content. These come from GitHub
// with a line number at the PR head and are never edited here, so
// they are placed by that number alone. A checkout that isn't at the
// PR head shows them a few lines off, the same way GitHub marks a
// thread outdated.

import { getChunks, getOriginalDoc } from '@codemirror/merge';
import { Compartment, Facet, StateField, type EditorState, type Extension, type Range } from '@codemirror/state';
import { Decoration, EditorView, WidgetType, type DecorationSet } from '@codemirror/view';
import { openUrl } from '@tauri-apps/plugin-opener';
import { relativeTime } from '../inboxSections';
import { githubMarkdown, repoUrlOf } from '../githubMarkdown';
import type { PrThreadComment, PullRequestDetail, ReviewSide } from '../protocol';
import { blockCards, blockShell } from './blockCards';
import { diffCollapsePins } from './diffCollapse';

/** What the agent said about a line, and what the user asked back
 *  (ADR 0124). Local to the IDE: nothing here is on GitHub. */
export type AgentThread = {
	entries: readonly { from: 'agent' | 'you'; text: string }[];
	/** The agent has been asked and hasn't answered yet. */
	pending: boolean;
	onReply: (text: string) => void;
	onDismiss: () => void;
};

export type PrAnnotation = (
	| { kind: 'note'; line: number; text: string }
	| { kind: 'thread'; line: number; resolved: boolean; comments: PrThreadComment[] }
	| { kind: 'agent'; line: number; thread: AgentThread }
) & {
	/** The pull request's repository: where `#123` in the text points. */
	repoUrl: string | null;
};

/** Annotations that sit on a line of `path`'s `side`. File-level
 * notes and outdated threads have no line; the PR panel lists those. */
export function annotationsFor(pr: PullRequestDetail | null, path: string, side: ReviewSide): PrAnnotation[] {
	if (pr === null) {
		return [];
	}
	const repoUrl = repoUrlOf(pr.url);
	const out: PrAnnotation[] = [];
	for (const thread of pr.threads) {
		if (thread.path === path && thread.side === side && thread.line !== null) {
			out.push({ kind: 'thread', line: thread.line, resolved: thread.resolved, comments: thread.comments, repoUrl });
		}
	}
	if (side === 'working') {
		for (const note of pr.minorNotes) {
			if (note.path === path && note.line !== null) {
				out.push({ kind: 'note', line: note.line, text: note.text, repoUrl });
			}
		}
	}
	return out;
}

const prAnnotationsFacet = Facet.define<readonly PrAnnotation[], readonly PrAnnotation[]>({
	combine: (values) => values[0] ?? [],
});

/** Base-side annotations for a unified diff, where the base document
 * has no pane of its own: their `line` counts in the original text. */
const prBaseAnnotationsFacet = Facet.define<readonly PrAnnotation[], readonly PrAnnotation[]>({
	combine: (values) => values[0] ?? [],
});

/** Markdown links inside a card open in the system browser instead
 * of navigating the webview. */
function openLinks(event: MouseEvent): void {
	const anchor = event.target instanceof Element ? event.target.closest('a') : null;
	if (anchor === null) {
		return;
	}
	event.preventDefault();
	const href = anchor.getAttribute('href') ?? '';
	if (/^https?:\/\//.test(href)) {
		void openUrl(href);
	}
}

function el(tag: string, className: string, text?: string): HTMLElement {
	const node = document.createElement(tag);
	node.className = className;
	if (text !== undefined) {
		node.textContent = text;
	}
	return node;
}

function agentCard(thread: AgentThread, repoUrl: string | null): HTMLElement {
	const card = el('div', 'cm-pr-card cm-pr-agent');
	card.addEventListener('click', openLinks);
	const head = el('div', 'cm-pr-head');
	head.append(el('span', 'cm-pr-tag', 'agent'));
	head.append(el('span', 'cm-pr-meta', 'only you see this'));
	head.append(el('span', 'cm-pr-spacer'));
	const dismiss = el('button', 'cm-pr-link', 'Dismiss');
	dismiss.setAttribute('type', 'button');
	dismiss.addEventListener('click', () => thread.onDismiss());
	head.append(dismiss);
	card.append(head);
	const list = el('div', 'cm-pr-comments');
	for (const entry of thread.entries) {
		const item = el('div', 'cm-pr-comment');
		if (entry.from === 'you') {
			item.append(el('span', 'cm-pr-author', 'you'), el('span', 'cm-pr-asked', entry.text));
		} else {
			const body = el('div', 'cm-pr-body markdown-body');
			githubMarkdown(body, { text: entry.text, repoUrl });
			item.append(body);
		}
		list.append(item);
	}
	if (thread.pending) {
		list.append(el('div', 'cm-pr-comment cm-pr-pending', 'The agent is looking…'));
	}
	card.append(list);
	const reply = document.createElement('input');
	reply.className = 'cm-pr-reply';
	reply.type = 'text';
	reply.placeholder = 'Ask the agent about this…';
	reply.spellcheck = false;
	reply.addEventListener('keydown', (event) => {
		event.stopPropagation();
		const text = reply.value.trim();
		if (event.key !== 'Enter' || text === '') {
			return;
		}
		event.preventDefault();
		reply.value = '';
		thread.onReply(text);
	});
	card.append(reply);
	return card;
}

class AnnotationWidget extends WidgetType {
	/** `oldLine`: the annotation is about a line of the base text that
	 * this editor does not number, so the card says which. */
	constructor(
		private readonly annotation: PrAnnotation,
		private readonly oldLine: number | null = null,
	) {
		super();
	}

	override eq(other: AnnotationWidget): boolean {
		return other.annotation === this.annotation && other.oldLine === this.oldLine;
	}

	override toDOM(): HTMLElement {
		const annotation = this.annotation;
		if (annotation.kind === 'note') {
			const card = el('div', 'cm-pr-card cm-pr-note');
			card.addEventListener('click', openLinks);
			card.append(el('span', 'cm-pr-tag', 'moonbot note'));
			const body = el('div', 'cm-pr-body markdown-body');
			githubMarkdown(body, { text: annotation.text, repoUrl: annotation.repoUrl });
			card.append(body);
			return blockShell(card);
		}
		if (annotation.kind === 'agent') {
			return blockShell(agentCard(annotation.thread, annotation.repoUrl));
		}
		const card = el('div', 'cm-pr-card cm-pr-thread');
		card.addEventListener('click', openLinks);
		const head = el('div', 'cm-pr-head');
		const first = annotation.comments[0];
		head.append(el('span', 'cm-pr-tag', annotation.resolved ? 'resolved thread' : 'review thread'));
		if (this.oldLine !== null) {
			const old = el('span', 'cm-pr-oldline', `old line ${this.oldLine}`);
			old.title = 'This thread is on a line of the base version';
			head.append(old);
		}
		const count = annotation.comments.length;
		head.append(el('span', 'cm-pr-meta', `${first?.author ?? 'someone'}${count > 1 ? ` + ${count - 1} more` : ''}`));
		head.append(el('span', 'cm-pr-spacer'));
		if (first !== undefined) {
			const link = el('button', 'cm-pr-link', 'GitHub');
			link.setAttribute('type', 'button');
			link.title = 'Open this thread on GitHub';
			link.addEventListener('click', () => void openUrl(first.url));
			head.append(link);
		}
		card.append(head);
		const list = el('div', 'cm-pr-comments');
		const now = Date.now();
		for (const comment of annotation.comments) {
			const item = el('div', 'cm-pr-comment');
			const meta = el('div', 'cm-pr-comment-meta');
			meta.append(el('span', 'cm-pr-author', comment.author ?? 'someone'));
			meta.append(el('span', 'cm-pr-meta', relativeTime(comment.createdAt, now)));
			const body = el('div', 'cm-pr-body markdown-body');
			githubMarkdown(body, { text: comment.body, repoUrl: annotation.repoUrl });
			item.append(meta, body);
			list.append(item);
		}
		card.append(list);
		// A resolved thread starts folded to its header; the header
		// toggles it.
		if (annotation.resolved) {
			card.classList.add('cm-pr-folded');
			head.addEventListener('click', (event) => {
				if (!(event.target instanceof HTMLButtonElement)) {
					card.classList.toggle('cm-pr-folded');
				}
			});
		}
		return blockShell(card);
	}

	override ignoreEvent(): boolean {
		return true;
	}
}

/** Where a base-side annotation goes in a unified diff. A removed
 * line has no line of its own there: the card sits right under the
 * removed chunk it belongs to (`removedAt`). An unchanged one still
 * exists in the working text, at `line`. */
type BasePlacement = { removedAt: number } | { line: number };

function placeBaseAnnotation(state: EditorState, oldLine: number): BasePlacement | null {
	const chunks = getChunks(state)?.chunks;
	if (chunks === undefined) {
		return null;
	}
	const original = getOriginalDoc(state);
	if (oldLine < 1 || oldLine > original.lines) {
		return null;
	}
	const pos = original.line(oldLine).from;
	// Offset between the two texts in the unchanged run `pos` is in.
	let shift = 0;
	for (const chunk of chunks) {
		if (pos < chunk.fromA) {
			break;
		}
		if (pos < chunk.toA) {
			return { removedAt: Math.min(chunk.fromB, state.doc.length) };
		}
		shift = chunk.toB - chunk.toA;
	}
	const mapped = pos + shift;
	return mapped > state.doc.length ? null : { line: state.doc.lineAt(mapped).number };
}

function buildDecorations(state: EditorState): DecorationSet {
	const ranges: Range<Decoration>[] = [];
	for (const annotation of state.facet(prAnnotationsFacet)) {
		if (annotation.line < 1 || annotation.line > state.doc.lines) {
			continue;
		}
		ranges.push(
			Decoration.widget({ widget: new AnnotationWidget(annotation), side: 2, block: true }).range(
				state.doc.line(annotation.line).to,
			),
		);
	}
	for (const annotation of state.facet(prBaseAnnotationsFacet)) {
		const place = placeBaseAnnotation(state, annotation.line);
		if (place === null) {
			continue;
		}
		const widget = new AnnotationWidget(annotation, annotation.line);
		if ('line' in place) {
			ranges.push(Decoration.widget({ widget, side: 2, block: true }).range(state.doc.line(place.line).to));
			continue;
		}
		// Side 0 of a block widget still means "before the line", but
		// after the removed chunk's own widget (side -1) at that spot.
		ranges.push(Decoration.widget({ widget, side: 0, block: true }).range(place.removedAt));
	}
	return Decoration.set(ranges, true);
}

function inputsChanged(before: EditorState, after: EditorState): boolean {
	return (
		before.facet(prAnnotationsFacet) !== after.facet(prAnnotationsFacet) ||
		before.facet(prBaseAnnotationsFacet) !== after.facet(prBaseAnnotationsFacet) ||
		(after.facet(prBaseAnnotationsFacet).length > 0 && getChunks(before)?.chunks !== getChunks(after)?.chunks)
	);
}

const prAnnotationsField = StateField.define<DecorationSet>({
	create: (state) => buildDecorations(state),
	update: (value, tr) => {
		if (!tr.docChanged && !inputsChanged(tr.startState, tr.state)) {
			return value;
		}
		return buildDecorations(tr.state);
	},
	provide: (field) => EditorView.decorations.from(field),
});

// An annotated line stays out of the diff's collapsed runs. A thread
// on a removed line needs no pin: changes are never collapsed.
const annotationPins = diffCollapsePins.compute([prAnnotationsFacet, prBaseAnnotationsFacet, 'doc'], (state) => {
	const lines = state.facet(prAnnotationsFacet).map((annotation) => annotation.line);
	for (const annotation of state.facet(prBaseAnnotationsFacet)) {
		const place = placeBaseAnnotation(state, annotation.line);
		if (place !== null && 'line' in place) {
			lines.push(place.line);
		}
	}
	return lines.filter((line) => line >= 1 && line <= state.doc.lines);
});

// The shell (width, inset, border, surface) is the shared one in
// `blockCards.ts`; this is the inside of a pull-request card and the
// accent that tells its kind apart.
const prAnnotationsTheme = EditorView.baseTheme({
	'.cm-pr-card': {
		padding: '6px 10px',
		'--m-card-accent': 'var(--m-warning)',
	},
	'.cm-pr-note': {
		display: 'flex',
		alignItems: 'baseline',
		gap: '8px',
		'--m-card-accent': 'var(--m-merged)',
	},
	'.cm-pr-agent': {
		'--m-card-accent': 'var(--m-accent)',
	},
	'.cm-pr-asked': {
		marginLeft: '8px',
		color: 'var(--m-fg-muted)',
		whiteSpace: 'pre-wrap',
	},
	'.cm-pr-pending': {
		color: 'var(--m-fg-subtle)',
	},
	'.cm-pr-reply': {
		boxSizing: 'border-box',
		width: '100%',
		marginTop: '6px',
		padding: '3px 6px',
		border: '1px solid var(--m-border)',
		borderRadius: 'var(--m-radius-sm)',
		background: 'var(--m-bg)',
		color: 'var(--m-fg)',
		font: 'inherit',
	},
	'.cm-pr-reply:focus': {
		outline: 'none',
		borderColor: 'var(--m-accent)',
	},
	'.cm-pr-thread.cm-pr-folded': {
		'--m-card-accent': 'var(--m-border-strong)',
	},
	'.cm-pr-folded .cm-pr-comments': {
		display: 'none',
	},
	'.cm-pr-folded .cm-pr-head': {
		cursor: 'pointer',
	},
	'.cm-pr-head, .cm-pr-comment-meta': {
		display: 'flex',
		alignItems: 'baseline',
		gap: '8px',
	},
	'.cm-pr-tag': {
		flexShrink: '0',
		fontSize: '10px',
		fontWeight: '600',
		letterSpacing: '0.05em',
		textTransform: 'uppercase',
		color: 'var(--m-fg-muted)',
	},
	'.cm-pr-meta, .cm-pr-oldline': {
		color: 'var(--m-fg-subtle)',
		fontSize: 'var(--m-text-xs)',
	},
	'.cm-pr-oldline': {
		flexShrink: '0',
		fontVariantNumeric: 'tabular-nums',
	},
	'.cm-pr-author': {
		fontWeight: '600',
	},
	'.cm-pr-spacer': {
		flex: '1',
	},
	'.cm-pr-link': {
		padding: '0 6px',
		border: '1px solid var(--m-border)',
		borderRadius: 'var(--m-radius-sm)',
		background: 'transparent',
		color: 'var(--m-fg-muted)',
		font: 'inherit',
		fontSize: 'var(--m-text-xs)',
		cursor: 'pointer',
	},
	'.cm-pr-link:hover': {
		color: 'var(--m-fg)',
		borderColor: 'var(--m-border-strong)',
	},
	'.cm-pr-comment': {
		marginTop: '6px',
	},
	'.cm-pr-comment + .cm-pr-comment': {
		paddingTop: '6px',
		borderTop: '1px solid var(--m-border)',
	},
	'.cm-pr-body': {
		minWidth: '0',
	},
	'.cm-pr-body > :first-child': {
		marginTop: '0',
	},
	'.cm-pr-body > :last-child': {
		marginBottom: '0',
	},
});

const NO_ANNOTATIONS: readonly PrAnnotation[] = [];

/** One editor's annotation plumbing: the extension to install and a
 * way to push a new list in when the PR is re-read. `unifiedBase` is
 * for a unified diff, which shows the base side's annotations too. */
export class PrAnnotationWiring {
	private readonly compartment = new Compartment();
	private view: EditorView | null = null;

	extension(initial: readonly PrAnnotation[], unifiedBase = NO_ANNOTATIONS): Extension {
		return [
			this.compartment.of(this.facets(initial, unifiedBase)),
			prAnnotationsField,
			annotationPins,
			prAnnotationsTheme,
			blockCards,
		];
	}

	attach(view: EditorView): void {
		this.view = view;
	}

	detach(): void {
		this.view = null;
	}

	sync(annotations: readonly PrAnnotation[], unifiedBase = NO_ANNOTATIONS): void {
		this.view?.dispatch({ effects: this.compartment.reconfigure(this.facets(annotations, unifiedBase)) });
	}

	private facets(annotations: readonly PrAnnotation[], unifiedBase: readonly PrAnnotation[]): Extension {
		return [prAnnotationsFacet.of(annotations), prBaseAnnotationsFacet.of(unifiedBase)];
	}
}
