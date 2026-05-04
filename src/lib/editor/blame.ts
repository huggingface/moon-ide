// Inline git-blame annotation for the current line, GitLens-style.
//
// The extension consumes a `GitFileBlame` via the `blameFacet`
// (reconfigured by the Editor component whenever the workspace's
// cached blame for the active buffer changes). A ViewPlugin paints
// a dim widget at end-of-line for the caret's current line with
// author + relative date + commit summary, and a hover on the
// widget spawns a floating popover with the full metadata.
//
// Design notes:
//
// - Current-line only, not every line. GitLens and friends default
//   to the same: per-line annotations at every row are
//   visually overwhelming, and the author's "whose commit is this?"
//   question is almost always about the line they're reading.
// - Widget decoration rather than line decoration so the CSS pixels
//   don't shift the doc content around. `side: 1` anchors it after
//   any text on the line; reading order matches the screen left-
//   to-right.
// - Popover lives at document.body level with a short
//   hover-intent delay. Keyboard users can ignore it entirely —
//   nothing about the blame pipeline relies on hover being
//   triggered.
// - When the buffer has edits that shift line numbers, blame data
//   is stale until the next save re-runs `git blame`. We still
//   show it; a per-line mismatch is annoying but less so than
//   having the indicator flicker off whenever the user types.

import { Facet, type Extension } from '@codemirror/state';
import { Decoration, type DecorationSet, EditorView, ViewPlugin, type ViewUpdate, WidgetType } from '@codemirror/view';
import { openUrl } from '@tauri-apps/plugin-opener';
import type { GitFileBlame, GitLineBlame } from '../protocol';

/**
 * Facet the Editor fills from `workspace.blameByPath`. `null` means
 * "no blame available for this file" (non-repo / untracked / not
 * yet fetched) — the ViewPlugin treats that as a no-op and skips
 * rendering.
 *
 * `combine` prefers the last non-null contribution. This matters
 * because the Editor reconfigures its compartment with new blame
 * data *on top of* whatever default the extension itself provides;
 * an `at(0)`-style pick would always see the default first and
 * swallow the live value, which is what nearly shipped with this.
 */
export const blameFacet = Facet.define<GitFileBlame | null, GitFileBlame | null>({
	combine: (values) => values.findLast((v) => v !== null) ?? null,
});

/** One-character width gap between code and annotation. Kept in one
 *  place so tweaking the visual weight doesn't drift. */
const GAP = '    ';

class BlameWidget extends WidgetType {
	constructor(
		readonly entry: GitLineBlame,
		/**
		 * Canonical web URL of the repo's origin (e.g.
		 * `https://github.com/moon/ide`) or `""` when unavailable.
		 * Threaded through so the hover tooltip can turn `#NNN`
		 * references in the commit subject into clickable PR links
		 * without the inline badge having to re-query any state.
		 */
		readonly remoteUrl: string,
	) {
		super();
	}

	toDOM(): HTMLElement {
		const span = document.createElement('span');
		span.className = 'cm-blame';
		// Gap is part of the widget's own content rather than CSS
		// padding so the hover hit-box starts at the annotation
		// text, not at the gap before it. Otherwise mousing over
		// the whitespace between code and badge would pop the
		// tooltip open.
		const gap = document.createElement('span');
		gap.className = 'cm-blame-gap';
		gap.textContent = GAP;
		span.appendChild(gap);
		const text = document.createElement('span');
		text.className = 'cm-blame-text';
		text.textContent = formatInline(this.entry);
		span.appendChild(text);
		// Hover plumbing. Delays match VS Code / GitLens roughly:
		// 400 ms open, 120 ms close. Shorter feels twitchy, longer
		// feels unresponsive when you really want the tooltip.
		let hoverTimer: ReturnType<typeof setTimeout> | null = null;
		text.addEventListener('mouseenter', () => {
			if (hoverTimer !== null) {
				clearTimeout(hoverTimer);
			}
			hoverTimer = setTimeout(() => showBlameHover(text, this.entry, this.remoteUrl), 400);
		});
		text.addEventListener('mouseleave', () => {
			if (hoverTimer !== null) {
				clearTimeout(hoverTimer);
				hoverTimer = null;
			}
			scheduleHideBlameHover();
		});
		return span;
	}

	/// Two widgets are interchangeable when they'd render identical
	/// text. Lets CM reuse DOM across unrelated decoration rebuilds
	/// (every selection-set triggers one) instead of tearing down
	/// and re-creating the span. Critical for the tooltip's hover
	/// state: a tear-down would fire `mouseleave` and kill an
	/// already-open tooltip whenever the user arrowed around within
	/// the same line block.
	override eq(other: WidgetType): boolean {
		if (!(other instanceof BlameWidget)) {
			return false;
		}
		const a = this.entry;
		const b = other.entry;
		return (
			a.sha === b.sha &&
			a.author === b.author &&
			a.summary === b.summary &&
			a.isUncommitted === b.isUncommitted &&
			a.authorTime === b.authorTime &&
			this.remoteUrl === other.remoteUrl
		);
	}

	/// The widget is purely informational. Let clicks and pointer
	/// events pass through so CM's own caret placement on the
	/// underlying line still works. Without this, a click near the
	/// annotation would swallow the caret move and confuse users
	/// trying to land at end of line.
	override ignoreEvent(): boolean {
		return false;
	}
}

/**
 * Inline text for the widget: `{author}, {relative date} • {summary}`.
 * Summary truncates with an ellipsis at ~60 chars so the annotation
 * doesn't take over the viewport on commits with novel-length subjects.
 */
function formatInline(entry: GitLineBlame): string {
	if (entry.isUncommitted) {
		return 'Uncommitted changes';
	}
	const author = entry.author || 'Unknown';
	const date = formatRelativeDate(entry.authorTime);
	const summary = entry.summary.length > 60 ? `${entry.summary.slice(0, 57)}…` : entry.summary;
	return `${author}, ${date} • ${summary}`;
}

/**
 * Humanized time-ago. Buckets match VS Code / GitLens phrasing so
 * the inline badge doesn't look out of place next to the ones users
 * already know. All bucket widths use SI multiples (60/60/24/7/30/365)
 * — approximate is fine, the absolute date is one hover away.
 */
function formatRelativeDate(authorTime: number): string {
	if (authorTime <= 0) {
		return 'unknown';
	}
	const now = Date.now() / 1000;
	const delta = Math.max(0, now - authorTime);
	if (delta < 60) {
		return 'just now';
	}
	if (delta < 3600) {
		const m = Math.floor(delta / 60);
		return m === 1 ? 'a minute ago' : `${m} minutes ago`;
	}
	if (delta < 86_400) {
		const h = Math.floor(delta / 3600);
		return h === 1 ? 'an hour ago' : `${h} hours ago`;
	}
	if (delta < 86_400 * 7) {
		const d = Math.floor(delta / 86_400);
		if (d === 1) {
			return 'yesterday';
		}
		return `${d} days ago`;
	}
	if (delta < 86_400 * 30) {
		const w = Math.floor(delta / (86_400 * 7));
		if (w === 1) {
			return 'last week';
		}
		return `${w} weeks ago`;
	}
	if (delta < 86_400 * 365) {
		const mo = Math.floor(delta / (86_400 * 30));
		if (mo === 1) {
			return 'last month';
		}
		return `${mo} months ago`;
	}
	const y = Math.floor(delta / (86_400 * 365));
	if (y === 1) {
		return 'last year';
	}
	return `${y} years ago`;
}

/**
 * Absolute date for the tooltip. Uses the user's locale (no
 * hardcoded format string) so a French user sees `4 mai 2026 à
 * 14:32` and an American sees `May 4, 2026, 2:32 PM`. The relative
 * date below it gives the "how long ago" answer either way.
 */
function formatAbsoluteDate(authorTime: number): string {
	if (authorTime <= 0) {
		return 'unknown';
	}
	const d = new Date(authorTime * 1000);
	return d.toLocaleString(undefined, {
		year: 'numeric',
		month: 'short',
		day: 'numeric',
		hour: '2-digit',
		minute: '2-digit',
	});
}

// Single global tooltip — only one blame hover can be open at a
// time, and sharing the element avoids leaking detached nodes on
// rapid buffer switches. Scoped to module state rather than
// WeakMap'd to views because the tooltip lives outside any
// particular view's DOM tree.
let activeHover: HTMLDivElement | null = null;
let hideTimer: ReturnType<typeof setTimeout> | null = null;

function showBlameHover(anchor: HTMLElement, entry: GitLineBlame, remoteUrl: string): void {
	hideBlameHover();
	const div = document.createElement('div');
	div.className = 'cm-blame-hover';
	div.setAttribute('role', 'tooltip');
	div.appendChild(renderHoverContent(entry, remoteUrl));
	document.body.appendChild(div);
	activeHover = div;

	// Position: prefer above the anchor; fall back to below when
	// there's no room (bottom of the editor, bottom of the screen,
	// etc.). Use the anchor's bounding rect rather than the mouse
	// position so the tooltip doesn't jitter when the user moves
	// within the annotation.
	positionHover(div, anchor);

	// Keep the popover open while the pointer is on *it* too,
	// even though the widget already moved out and its leave fired.
	// Without this hand-off you can't click links / select text in
	// the tooltip without it vanishing under your cursor.
	div.addEventListener('mouseenter', () => {
		if (hideTimer !== null) {
			clearTimeout(hideTimer);
			hideTimer = null;
		}
	});
	div.addEventListener('mouseleave', () => scheduleHideBlameHover());
}

function scheduleHideBlameHover(): void {
	if (hideTimer !== null) {
		clearTimeout(hideTimer);
	}
	hideTimer = setTimeout(() => {
		hideTimer = null;
		hideBlameHover();
	}, 120);
}

function hideBlameHover(): void {
	if (hideTimer !== null) {
		clearTimeout(hideTimer);
		hideTimer = null;
	}
	if (activeHover) {
		activeHover.remove();
		activeHover = null;
	}
}

function positionHover(div: HTMLDivElement, anchor: HTMLElement): void {
	const rect = anchor.getBoundingClientRect();
	// Measure after the node is in the DOM, so layout has run and
	// offsetHeight is real.
	const h = div.offsetHeight;
	const w = div.offsetWidth;
	const above = rect.top - h - 8;
	const below = rect.bottom + 8;
	const top = above > 8 ? above : below;
	let left = rect.left;
	// Keep it inside the viewport horizontally.
	if (left + w > window.innerWidth - 8) {
		left = Math.max(8, window.innerWidth - w - 8);
	}
	div.style.top = `${Math.round(top + window.scrollY)}px`;
	div.style.left = `${Math.round(left + window.scrollX)}px`;
}

function renderHoverContent(entry: GitLineBlame, remoteUrl: string): HTMLElement {
	const frag = document.createElement('div');
	if (entry.isUncommitted) {
		const title = document.createElement('div');
		title.className = 'cm-blame-hover-title';
		title.textContent = 'Uncommitted changes';
		frag.appendChild(title);
		const body = document.createElement('div');
		body.className = 'cm-blame-hover-meta';
		body.textContent = 'Local edits not yet committed.';
		frag.appendChild(body);
		return frag;
	}
	const title = document.createElement('div');
	title.className = 'cm-blame-hover-title';
	title.appendChild(linkifyCommitText(entry.summary, remoteUrl));
	frag.appendChild(title);

	const meta = document.createElement('div');
	meta.className = 'cm-blame-hover-meta';
	const author = document.createElement('div');
	author.textContent = entry.authorEmail ? `${entry.author} <${entry.authorEmail}>` : entry.author;
	meta.appendChild(author);
	const dateLine = document.createElement('div');
	dateLine.textContent = `${formatAbsoluteDate(entry.authorTime)} · ${formatRelativeDate(entry.authorTime)}`;
	meta.appendChild(dateLine);
	const shaLine = document.createElement('div');
	shaLine.textContent = `commit ${entry.sha.slice(0, 8)}`;
	meta.appendChild(shaLine);
	frag.appendChild(meta);

	// Show the full commit message only when it differs from the
	// subject. The backend currently fills `message` with the
	// subject because `git blame --porcelain` doesn't ship the body
	// (see the Rust-side note); this branch lights up once we wire
	// a secondary `git show %B` lookup per unique sha.
	if (entry.message && entry.message.trim() !== entry.summary.trim()) {
		const body = document.createElement('div');
		body.className = 'cm-blame-hover-body';
		body.appendChild(linkifyCommitText(entry.message, remoteUrl));
		frag.appendChild(body);
	}
	return frag;
}

/**
 * Turn `#NNN` and `owner/repo#NNN` references in a commit subject or
 * body into clickable anchors that open the corresponding PR in the
 * user's default browser. Plain `#NNN` is only linkified when
 * `remoteUrl` is non-empty — otherwise we have nothing to link to
 * and leave the text alone.
 *
 * The returned fragment is a mix of text and anchor nodes; drop it
 * into any container element. Using a fragment (rather than
 * `innerHTML`) keeps the path XSS-safe — summaries are attacker-
 * controlled in the "someone can push to the repo" sense, and
 * injecting them as HTML was how I originally wrote this before
 * catching the obvious vulnerability.
 *
 * Match shapes handled:
 * - `#123` (repo-local) → `${remoteUrl}/pull/123`
 * - `owner/repo#123` (cross-repo) → `https://github.com/owner/repo/pull/123`
 *
 * The 1-7 digit cap is loose enough to cover any realistic PR number
 * (GitHub's largest is ~300k today) but rejects hex SHAs, colour
 * codes, and similar false positives that start with `#` and run
 * long.
 */
function linkifyCommitText(text: string, remoteUrl: string): DocumentFragment {
	const frag = document.createDocumentFragment();
	if (!text) {
		return frag;
	}
	const re = /([A-Za-z0-9][A-Za-z0-9._-]*\/[A-Za-z0-9][A-Za-z0-9._-]*)?#(\d{1,7})/g;
	let last = 0;
	for (const match of text.matchAll(re)) {
		const idx = match.index ?? 0;
		const [full, repo, num] = match;
		const prev = idx > 0 ? (text[idx - 1] ?? '') : '';
		const after = idx + full.length;
		const next = text[after] ?? '';
		// Reject a bare `#NNN` whose leading character glues it to
		// another identifier (`abc#123`). For `owner/repo#NNN` we
		// keep the whole match because that's a legitimate shape.
		if (!repo && /[A-Za-z0-9_]/.test(prev)) {
			continue;
		}
		// Reject trailing alnum — `#12345abc` is almost certainly a
		// commit-ish, not a PR number.
		if (/[A-Za-z0-9]/.test(next)) {
			continue;
		}
		const href = repo ? `https://github.com/${repo}/pull/${num}` : remoteUrl ? `${remoteUrl}/pull/${num}` : null;
		if (!href) {
			continue;
		}
		if (idx > last) {
			frag.appendChild(document.createTextNode(text.slice(last, idx)));
		}
		const a = document.createElement('a');
		a.href = href;
		a.className = 'cm-blame-pr-link';
		a.rel = 'noopener noreferrer';
		a.textContent = full;
		// Open via the Tauri opener plugin rather than letting the
		// webview follow the link — a raw navigation inside the app
		// window would blow away the IDE. `preventDefault` covers
		// both middle-click and Cmd/Ctrl-click, which fire `click`
		// in Chromium.
		a.addEventListener('click', (ev) => {
			ev.preventDefault();
			void openUrl(href);
		});
		frag.appendChild(a);
		last = after;
	}
	if (last < text.length) {
		frag.appendChild(document.createTextNode(text.slice(last)));
	}
	return frag;
}

/**
 * The ViewPlugin that watches blame + selection + doc-change and
 * rebuilds the current-line decoration. Cheap: one widget per
 * rebuild, no per-line scan.
 */
const blamePlugin = ViewPlugin.fromClass(
	class {
		decorations: DecorationSet;

		constructor(view: EditorView) {
			this.decorations = buildDecorations(view);
		}

		update(update: ViewUpdate): void {
			const oldBlame = update.startState.facet(blameFacet);
			const newBlame = update.state.facet(blameFacet);
			if (update.docChanged || update.selectionSet || oldBlame !== newBlame) {
				this.decorations = buildDecorations(update.view);
			}
		}

		destroy(): void {
			hideBlameHover();
		}
	},
	{
		decorations: (v) => v.decorations,
	},
);

function buildDecorations(view: EditorView): DecorationSet {
	const blame = view.state.facet(blameFacet);
	if (!blame || blame.lines.length === 0) {
		return Decoration.none;
	}
	const head = view.state.selection.main.head;
	const line = view.state.doc.lineAt(head);
	// The buffer can grow past the last-saved blame when the user
	// types newlines without saving; those lines legitimately have
	// no entry. Normal; the widget reappears after save.
	const entry = blame.lines[line.number - 1];
	if (!entry) {
		return Decoration.none;
	}
	// Skip entries the backend couldn't fill (malformed porcelain /
	// parser gave up). An empty author string is a clearer "no data"
	// signal than rendering `, unknown • `.
	if (!entry.author && !entry.isUncommitted) {
		return Decoration.none;
	}
	const widget = Decoration.widget({
		widget: new BlameWidget(entry, blame.remoteUrl),
		side: 1,
	}).range(line.to);
	return Decoration.set([widget]);
}

/**
 * Bundle for the Editor: facet (so callers can reconfigure the
 * blame data), plugin (renders the widget), and the base theme
 * contribution for the `.cm-blame*` selectors. Theme rules live
 * here so an editor without the blame extension doesn't pay for
 * unused CSS.
 */
export function blameExtension(): Extension {
	// No default facet contribution: the Editor always provides one
	// via its compartment. Adding a static `null` here would bloat
	// the facet's input list and require the `findLast` combine
	// above to skip it; simpler to leave it out entirely.
	return [blamePlugin, blameTheme];
}

const blameTheme = EditorView.baseTheme({
	'.cm-blame': {
		pointerEvents: 'none',
	},
	'.cm-blame-gap': {
		whiteSpace: 'pre',
	},
	'.cm-blame-text': {
		color: 'var(--m-fg-subtle)',
		fontStyle: 'italic',
		fontSize: '11px',
		pointerEvents: 'auto',
		cursor: 'default',
	},
	'.cm-blame-text:hover': {
		color: 'var(--m-fg-muted)',
	},
});
