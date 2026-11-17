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

import { Compartment, Facet, StateField, type EditorState, type Extension, type Range } from '@codemirror/state';
import { Decoration, EditorView, WidgetType, type DecorationSet } from '@codemirror/view';
import { openUrl } from '@tauri-apps/plugin-opener';
import { relativeTime } from '../inboxSections';
import { getCachedMarkdown, renderMarkdown } from '../markdown';
import type { PrThreadComment, PullRequestDetail, ReviewSide } from '../protocol';

export type PrAnnotation =
	| { kind: 'note'; line: number; text: string }
	| { kind: 'thread'; line: number; resolved: boolean; comments: PrThreadComment[] };

/** Annotations that sit on a line of `path`'s `side`. File-level
 * notes and outdated threads have no line; the PR panel lists those. */
export function annotationsFor(pr: PullRequestDetail | null, path: string, side: ReviewSide): PrAnnotation[] {
	if (pr === null) {
		return [];
	}
	const out: PrAnnotation[] = [];
	for (const thread of pr.threads) {
		if (thread.path === path && thread.side === side && thread.line !== null) {
			out.push({ kind: 'thread', line: thread.line, resolved: thread.resolved, comments: thread.comments });
		}
	}
	if (side === 'working') {
		for (const note of pr.minorNotes) {
			if (note.path === path && note.line !== null) {
				out.push({ kind: 'note', line: note.line, text: note.text });
			}
		}
	}
	return out;
}

const prAnnotationsFacet = Facet.define<readonly PrAnnotation[], readonly PrAnnotation[]>({
	combine: (values) => values[0] ?? [],
});

function renderMarkdownInto(target: HTMLElement, source: string): void {
	const cached = getCachedMarkdown(source);
	if (cached !== undefined) {
		target.innerHTML = cached;
		return;
	}
	target.textContent = source;
	void renderMarkdown(source).then((html) => {
		if (target.isConnected) {
			target.innerHTML = html;
		}
	});
}

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

class AnnotationWidget extends WidgetType {
	constructor(private readonly annotation: PrAnnotation) {
		super();
	}

	override eq(other: AnnotationWidget): boolean {
		return other.annotation === this.annotation;
	}

	override toDOM(): HTMLElement {
		const annotation = this.annotation;
		if (annotation.kind === 'note') {
			const card = el('div', 'cm-pr-card cm-pr-note');
			card.addEventListener('click', openLinks);
			card.append(el('span', 'cm-pr-tag', 'moonbot note'));
			const body = el('div', 'cm-pr-body markdown-body');
			renderMarkdownInto(body, annotation.text);
			card.append(body);
			return card;
		}
		const card = el('div', 'cm-pr-card cm-pr-thread');
		card.addEventListener('click', openLinks);
		const head = el('div', 'cm-pr-head');
		const first = annotation.comments[0];
		head.append(el('span', 'cm-pr-tag', annotation.resolved ? 'resolved thread' : 'review thread'));
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
			renderMarkdownInto(body, comment.body);
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
		return card;
	}

	override ignoreEvent(): boolean {
		return true;
	}
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
	return Decoration.set(ranges, true);
}

const prAnnotationsField = StateField.define<DecorationSet>({
	create: (state) => buildDecorations(state),
	update: (value, tr) => {
		if (!tr.docChanged && tr.startState.facet(prAnnotationsFacet) === tr.state.facet(prAnnotationsFacet)) {
			return value;
		}
		return buildDecorations(tr.state);
	},
	provide: (field) => EditorView.decorations.from(field),
});

const prAnnotationsTheme = EditorView.baseTheme({
	// Block widgets are as wide as the longest line, not as the pane.
	// Size the card against the scroller instead and pin it, so a
	// narrow pane wraps it rather than cutting it off.
	'& .cm-scroller': {
		containerType: 'inline-size',
	},
	'.cm-pr-card': {
		position: 'sticky',
		left: '28px',
		margin: '4px 8px 6px 28px',
		padding: '6px 10px',
		maxWidth: 'min(760px, calc(100cqw - 110px))',
		boxSizing: 'border-box',
		border: '1px solid var(--m-border)',
		borderLeft: '2px solid var(--m-warning)',
		borderRadius: 'var(--m-radius-md)',
		background: 'var(--m-bg-1)',
		fontFamily: 'var(--m-font-ui)',
		fontSize: 'var(--m-text-sm)',
		color: 'var(--m-fg)',
		whiteSpace: 'normal',
	},
	'.cm-pr-note': {
		display: 'flex',
		alignItems: 'baseline',
		gap: '8px',
		borderLeftColor: 'var(--m-merged)',
	},
	'.cm-pr-thread.cm-pr-folded': {
		borderLeftColor: 'var(--m-border-strong)',
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
	'.cm-pr-meta': {
		color: 'var(--m-fg-subtle)',
		fontSize: 'var(--m-text-xs)',
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

/** One editor's annotation plumbing: the extension to install and a
 * way to push a new list in when the PR is re-read. */
export class PrAnnotationWiring {
	private readonly compartment = new Compartment();
	private view: EditorView | null = null;

	extension(initial: readonly PrAnnotation[]): Extension {
		return [this.compartment.of(prAnnotationsFacet.of(initial)), prAnnotationsField, prAnnotationsTheme];
	}

	attach(view: EditorView): void {
		this.view = view;
	}

	detach(): void {
		this.view = null;
	}

	sync(annotations: readonly PrAnnotation[]): void {
		this.view?.dispatch({ effects: this.compartment.reconfigure(prAnnotationsFacet.of(annotations)) });
	}
}
