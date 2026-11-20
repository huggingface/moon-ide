<script lang="ts">
	import { onMount, untrack } from 'svelte';
	import { Compartment, EditorState, type Extension } from '@codemirror/state';
	import { EditorView, lineNumbers } from '@codemirror/view';
	import { foldGutter, indentUnit } from '@codemirror/language';
	import { highlightSelectionMatches } from '@codemirror/search';
	import { searchAsYouType } from '../editor/searchAsYouType';
	import { MergeView, diff as rawDiff, unifiedMergeView } from '@codemirror/merge';
	import { statusColor } from '../fileIcon';
	import { repoUrlOf } from '../githubMarkdown';
	import { ipc } from '../ipc';
	import { workspace, type SplitSide } from '../state.svelte';
	import { highlightTabs } from '../editor/highlightTabs';
	import { languageFor } from '../editor/language';
	import { moonEditorTheme } from '../editor/theme';
	import { diffPureChangeExtension } from '../editor/diffPureChange';
	import { diffGutterTintExtension } from '../editor/diffGutterTint';
	import { diffBlockAlign } from '../editor/diffBlockAlign';
	import { collapsedLines, diffCollapse, diffCollapseAsk, restoreCollapsed, revealLines } from '../editor/diffCollapse';
	import { findMarks, markRemovedHit, setFindMarks, type FindHit } from '../prFind';
	import { effectiveDiffLayout, gutterDigits } from '../editor/diffLayout';
	import { wireHorizontalScrollSync } from '../editor/hScrollSync';
	import { annotationsFor, PrAnnotationWiring, type PrAnnotation } from '../editor/prAnnotations';
	import type { FileOutline } from '../prFiles';
	import type { DiffLayout, EditorConfig, GitFileStatus, PullRequestDetail, ReviewSide } from '../protocol';
	import type { FileNotes, NoteThread } from '../reviewNotes';
	import DiffStat from './DiffStat.svelte';
	import FileIcon from './FileIcon.svelte';
	import StatusWord from './StatusWord.svelte';
	import ChevronDownIcon from './icons/ChevronDownIcon.svelte';
	import ChevronRightIcon from './icons/ChevronRightIcon.svelte';

	type Props = {
		path: string;
		status: GitFileStatus;
		// Full SHAs: the commit and its first parent. Both sides of
		// the diff read from `git_ref_content` — the base side from
		// `parentSha`, the after side from `commitSha`. An empty
		// `parentSha` (root commit) renders the base side empty.
		commitSha: string;
		parentSha: string;
		eager: boolean;
		registerSection: (path: string, el: HTMLElement | null) => void;
		side: SplitSide;
		// The pull request this diff is the "Files changed" of (the
		// PR tab, ADR 0113): its review threads and line-bound moonbot
		// notes render under their lines. Absent for a plain commit.
		pullRequest?: PullRequestDetail;
		// The PR tab's per-file review controls (ADR 0118): the size
		// of the change, a collapse toggle and a viewed mark. A
		// collapsed section is its header alone and loads nothing.
		outline?: FileOutline;
		// The bound folder whose repository holds both revisions, when
		// that need not be the active one: a pull request tab shows a
		// project's pull request wherever the user is (ADR 0121).
		folderPath?: string | undefined;
		// The agent's notes on this file and the ways to ask it more
		// (ADR 0124). Absent where there is no agent to ask.
		notes?: FileNotes | undefined;
		// The PR tab's find across files: hands it the way to show
		// this file's hits.
		registerFind?:
			| ((path: string, show: ((hits: FindHit[], current: number) => Promise<void>) | null) => void)
			| undefined;
	};

	let {
		path,
		status,
		commitSha,
		parentSha,
		eager,
		registerSection,
		side,
		pullRequest,
		outline,
		folderPath,
		notes,
		registerFind,
	}: Props = $props();

	const collapsed = $derived(outline?.collapsed ?? false);
	// Near enough to the viewport for its diff to be worth loading.
	let inView = $state(false);

	let sectionEl: HTMLElement | undefined = $state();
	let host: HTMLDivElement | undefined = $state();
	// Exactly one of `merge` / `unified` is set once built; `paneB` is
	// the after-side editor either way.
	let merge: MergeView | undefined = $state();
	let unified: EditorView | undefined = $state();
	const paneB = $derived.by<EditorView | undefined>(() => merge?.b ?? unified);
	let loading = $state(false);
	let buildToken = 0;
	let detachHScrollSync: (() => void) | null = null;

	// Both blobs are immutable, so they are fetched once and a layout
	// switch rebuilds the editors from them without another round trip.
	type Loaded = { base: string; after: string; lang: Extension };
	let loaded: Loaded | null = null;

	let hostWidth = $state(0);
	const layout = $derived(effectiveDiffLayout(workspace.diffLayout, hostWidth));
	let builtLayout = $state<DiffLayout | null>(null);

	const langA = new Compartment();
	const langB = new Compartment();
	const themeA = new Compartment();
	const themeB = new Compartment();
	const wrapA = new Compartment();
	const wrapB = new Compartment();
	// `tabSize` decides how wide a literal `\t` renders, so both
	// read-only panes still need the repo's editorconfig; without it
	// CodeMirror falls back to 4.
	const ecA = new Compartment();
	const ecB = new Compartment();

	const prWiringA = new PrAnnotationWiring();
	const prWiringB = new PrAnnotationWiring();
	// One card per thread object: a thread that didn't change keeps
	// its card, and whatever is half-typed in it.
	const noteCards = new WeakMap<NoteThread, PrAnnotation>();
	function noteAnnotations(threadSide: ReviewSide): PrAnnotation[] {
		const handlers = notes;
		if (handlers === undefined) {
			return [];
		}
		return handlers.threads
			.filter((thread) => thread.side === threadSide)
			.map((thread) => {
				let card = noteCards.get(thread);
				if (card === undefined) {
					card = {
						kind: 'agent',
						line: thread.line,
						repoUrl: pullRequest === undefined ? null : repoUrlOf(pullRequest.url),
						thread: {
							entries: thread.entries,
							pending: thread.pending,
							onReply: (text) => notes?.onReply(thread, text),
							onDismiss: () => notes?.onDismiss(thread),
						},
					};
					noteCards.set(thread, card);
				}
				return card;
			});
	}
	const baseAnnotations = $derived([...annotationsFor(pullRequest ?? null, path, 'base'), ...noteAnnotations('base')]);
	const workingAnnotations = $derived([
		...annotationsFor(pullRequest ?? null, path, 'working'),
		...noteAnnotations('working'),
	]);
	// The unified editor has no base pane, so it carries that side's
	// threads too, under the removed lines they are about.
	$effect(() => {
		prWiringA.sync(baseAnnotations);
	});
	$effect(() => {
		prWiringB.sync(workingAnnotations, builtLayout === 'unified' ? baseAnnotations : undefined);
	});

	function editorConfigExtensions(ec: EditorConfig): Extension {
		const unit = ec.indent_style === 'tab' ? '\t' : ' '.repeat(Math.max(1, ec.indent_size));
		return [EditorState.tabSize.of(Math.max(1, ec.tab_width)), indentUnit.of(unit)];
	}

	function fileName(p: string): string {
		const slash = p.lastIndexOf('/');
		return slash === -1 ? p : p.slice(slash + 1);
	}
	function dirName(p: string): string {
		const slash = p.lastIndexOf('/');
		return slash === -1 ? '' : p.slice(0, slash);
	}

	function firstLineOf(text: string): string {
		const idx = text.indexOf('\n');
		return idx === -1 ? text : text.slice(0, idx);
	}

	async function loadAtRev(rev: string): Promise<string> {
		if (rev.length === 0) {
			return '';
		}
		try {
			const content = await ipc.fs.gitRefContent(rev, path, folderPath);
			return content ?? '';
		} catch {
			return '';
		}
	}

	async function build() {
		if (loaded !== null || loading || !host) {
			return;
		}
		loading = true;
		const token = ++buildToken;
		// Base = parent blob, after = commit blob. For a root commit
		// `parentSha` is empty and the base side renders blank — every
		// file reads as a pure addition.
		const [base, after] = await Promise.all([loadAtRev(parentSha), loadAtRev(commitSha)]);
		if (token !== buildToken || !host) {
			return;
		}
		const firstLine = after.length > 0 ? firstLineOf(after) : firstLineOf(base);
		const [lang] = await Promise.all([languageFor(path, firstLine), workspace.ensureEditorConfig(path)]);
		if (token !== buildToken || !host) {
			return;
		}
		loaded = { base, after, lang };
		loading = false;
		if (!collapsed) {
			mountViews(loaded);
		}
	}

	// Collapsing drops the editors (hidden ones would measure
	// themselves at zero height) but keeps the blobs, so opening the
	// file again is a rebuild without a round trip.
	$effect(() => {
		const hide = collapsed;
		const wanted = inView;
		untrack(() => {
			if (hide) {
				destroyViews();
				return;
			}
			if (!wanted || builtLayout !== null) {
				return;
			}
			if (loaded !== null) {
				mountViews(loaded);
				return;
			}
			void build();
		});
	});

	function paneExtensions(pane: 'a' | 'b', lang: Extension): Extension[] {
		const isA = pane === 'a';
		// Selection-publish hook so `Ctrl+L` can attach a highlighted
		// range from the commit diff to the coder — same gesture as
		// the regular editor / DiffView / ReviewSection. Unlike those
		// surfaces, *both* panes here are historical blobs the user
		// might legitimately quote (removed code lives only on the
		// base side), so both sides publish. The attachment captures
		// the text verbatim, so quoting a blob that no longer matches
		// the working tree still hands the model the right bytes.
		const selectionPublish = EditorView.updateListener.of((update) => {
			if (update.selectionSet) {
				publishCommitSelection(update.state);
			}
		});
		return [
			lineNumbers(),
			diffGutterTintExtension(pane),
			diffCollapse(),
			// The new side's bars offer to explain the block they lead to.
			!isA && notes !== undefined ? diffCollapseAsk.of((block) => notes?.onExplain(block)) : [],
			foldGutter(),
			diffPureChangeExtension,
			highlightSelectionMatches(),
			searchAsYouType(),
			findMarks(),
			highlightTabs(),
			(isA ? themeA : themeB).of(moonEditorTheme(workspace.effectiveTheme)),
			(isA ? langA : langB).of(lang),
			(isA ? ecA : ecB).of(editorConfigExtensions(workspace.editorConfigFor(path))),
			(isA ? wrapA : wrapB).of(workspace.lineWrap ? EditorView.lineWrapping : []),
			selectionPublish,
			EditorState.readOnly.of(true),
			EditorView.editable.of(false),
		];
	}

	function mountViews({ base, after, lang }: Loaded) {
		if (!host) {
			return;
		}
		const next = layout;
		const annotated = pullRequest !== undefined;
		host.style.setProperty(
			'--m-diff-gutter-digits',
			String(gutterDigits(base.split('\n').length, after.split('\n').length)),
		);
		if (next === 'unified') {
			const view = new EditorView({
				parent: host,
				state: EditorState.create({
					doc: after,
					extensions: [
						...paneExtensions('b', lang),
						annotated ? prWiringB.extension(workingAnnotations, baseAnnotations) : [],
						unifiedMergeView({
							original: base,
							gutter: false,
							highlightChanges: true,
							syntaxHighlightDeletions: true,
							mergeControls: false,
							diffConfig: { override: rawDiff },
						}),
					],
				}),
			});
			if (annotated) {
				prWiringB.attach(view);
			}
			unified = view;
		} else {
			const view = new MergeView({
				a: {
					doc: base,
					extensions: [
						...paneExtensions('a', lang),
						diffBlockAlign,
						annotated ? prWiringA.extension(baseAnnotations) : [],
					],
				},
				b: {
					doc: after,
					extensions: [
						...paneExtensions('b', lang),
						diffBlockAlign,
						annotated ? prWiringB.extension(workingAnnotations) : [],
					],
				},
				parent: host,
				gutter: false,
				highlightChanges: true,
				diffConfig: { override: rawDiff },
			});
			detachHScrollSync = wireHorizontalScrollSync(view.a.scrollDOM, view.b.scrollDOM);
			if (annotated) {
				prWiringA.attach(view.a);
				prWiringB.attach(view.b);
			}
			merge = view;
		}
		builtLayout = next;
	}

	/** Marks this file's `hits` and brings `hits[current]` into view,
	 *  opening the diff and the collapsed lines it is in. No hits
	 *  clears the marks. */
	async function showFind(hits: FindHit[], current: number): Promise<void> {
		if (hits.length === 0) {
			merge?.a.dispatch({ effects: setFindMarks.of(null) });
			paneB?.dispatch({ effects: setFindMarks.of(null) });
			if (unified !== undefined) {
				markRemovedHit(unified, null);
			}
			return;
		}
		inView = true;
		// The editors are built once the file's text is in.
		for (let waited = 0; waited < 100; waited++) {
			if (paneB !== undefined) {
				break;
			}
			await new Promise((resolve) => setTimeout(resolve, 50));
		}
		const b = paneB;
		if (b === undefined) {
			return;
		}
		const a = merge?.a;
		const hit = hits[current];
		if (hit !== undefined) {
			const line = hit.side === 'b' ? hit.line : hit.anchor;
			restoreCollapsed(b, revealLines(collapsedLines(b.state), { from: line, to: line }));
		}
		const marksFor = (view: EditorView, pane: 'a' | 'b') => {
			const shown = hits.filter((one) => one.side === pane);
			const ranges = shown.map((one) => {
				const line = view.state.doc.line(Math.min(one.line, view.state.doc.lines));
				return { from: line.from + one.from, to: Math.min(line.to, line.from + one.to) };
			});
			view.dispatch({ effects: setFindMarks.of({ ranges, current: hit === undefined ? -1 : shown.indexOf(hit) }) });
		};
		marksFor(b, 'b');
		if (a !== undefined) {
			marksFor(a, 'a');
		}
		if (hit === undefined) {
			return;
		}
		// After the reveal has been laid out.
		await new Promise((resolve) => requestAnimationFrame(resolve));
		const target = hit.side === 'a' && a !== undefined ? a : b;
		const lineNo = target === b ? (hit.side === 'b' ? hit.line : hit.anchor) : hit.line;
		const line = target.state.doc.line(Math.min(Math.max(lineNo, 1), target.state.doc.lines));
		const pos = target === b && hit.side === 'a' ? line.from : Math.min(line.to, line.from + hit.from);
		// From the left edge, so a hit near the start of its line doesn't
		// show where the previous one, further right, left the pane.
		target.scrollDOM.scrollLeft = 0;
		target.dispatch({ effects: EditorView.scrollIntoView(pos, { y: 'center' }) });
		if (unified !== undefined) {
			const removedLine = markRemovedHit(unified, hit.side === 'a' ? hit : null);
			if (removedLine !== null) {
				await new Promise((resolve) => requestAnimationFrame(resolve));
				removedLine.scrollIntoView({ block: 'center' });
			}
		}
	}

	function destroyViews() {
		detachHScrollSync?.();
		detachHScrollSync = null;
		prWiringA.detach();
		prWiringB.detach();
		merge?.destroy();
		merge = undefined;
		unified?.destroy();
		unified = undefined;
		builtLayout = null;
	}

	// Where the layout switch reads "the line the reader is on": just
	// below the view's sticky banner and this section's sticky header.
	const ANCHOR_OFFSET_PX = 80;

	// The element whose scrollbar moves the stacked sections.
	function scrollParent(): HTMLElement | null {
		for (let el = sectionEl?.parentElement ?? null; el !== null; el = el.parentElement) {
			const overflow = getComputedStyle(el).overflowY;
			if (overflow === 'auto' || overflow === 'scroll') {
				return el;
			}
		}
		return null;
	}

	// Switching layout (preference flip, or the pane crossing the
	// narrow threshold) rebuilds the editors from the loaded blobs.
	// The two layouts differ in height, so the section the reader is
	// in keeps the line under the top of the viewport where it was;
	// the others just rebuild.
	$effect(() => {
		const next = layout;
		untrack(() => {
			if (builtLayout === null || builtLayout === next || loaded === null) {
				return;
			}
			const scroller = scrollParent();
			const before = paneB;
			let anchor: { line: number; top: number } | null = null;
			if (scroller !== null && before !== undefined) {
				const rect = before.dom.getBoundingClientRect();
				const top = scroller.getBoundingClientRect().top + ANCHOR_OFFSET_PX;
				if (rect.top <= top && rect.bottom > top) {
					const block = before.lineBlockAtHeight(top - before.documentTop);
					anchor = { line: before.state.doc.lineAt(block.from).number, top: before.documentTop + block.top };
				}
			}
			// Both layouts collapse the same lines of the after-side
			// text, so what the reader had expanded stays expanded.
			const hiddenLines = before === undefined ? null : collapsedLines(before.state);
			destroyViews();
			mountViews(loaded);
			const rebuilt = merge?.b ?? unified;
			if (hiddenLines !== null && rebuilt !== undefined) {
				restoreCollapsed(rebuilt, hiddenLines);
			}
			const token = buildToken;
			const held = anchor;
			if (held === null || scroller === null) {
				return;
			}
			// Two frames: the first lets the new editors measure and
			// the merge view place its spacers.
			requestAnimationFrame(() => {
				requestAnimationFrame(() => {
					const after = merge?.b ?? unified;
					if (token !== buildToken || after === undefined) {
						return;
					}
					const line = after.state.doc.line(Math.min(held.line, after.state.doc.lines));
					scroller.scrollTop += after.lineBlockAt(line.from).top + after.documentTop - held.top;
				});
			});
		});
	});

	// The file as it is on disk. For another project's pull request
	// that is a file of that project, so the click goes there.
	async function openInEditor() {
		if (folderPath !== undefined && workspace.activeProjectPath !== folderPath) {
			await workspace.setActiveFolder(folderPath);
			if (workspace.activeFolderPath !== folderPath) {
				return;
			}
		}
		await workspace.openFile(path, side);
	}

	/**
	 * Mirror of `ReviewSection.svelte`'s `publishReviewSelection`
	 * for the commit view's panes. Empty selections clear the
	 * snapshot only when it's ours, so a sibling section's (or the
	 * other pane's) fresh selection isn't stomped. Same off-by-one
	 * snap when the drag ends at the start of a line the user
	 * didn't actually mean to include.
	 */
	function publishCommitSelection(state: EditorState) {
		const sel = state.selection.main;
		if (sel.empty) {
			clearOurSelection();
			return;
		}
		const fromLine = state.doc.lineAt(sel.from);
		const toLine = state.doc.lineAt(sel.to);
		const effectiveToLineNumber =
			sel.to === toLine.from && toLine.number > fromLine.number ? toLine.number - 1 : toLine.number;
		const text = state.doc.sliceString(sel.from, sel.to);
		workspace.setActiveSelection({
			path,
			startLine: fromLine.number,
			endLine: effectiveToLineNumber,
			text,
		});
	}

	// Drop the workspace selection snapshot only when it belongs to
	// this section's path — keeps a sibling section's selection
	// alive when this one unmounts (lazy mount churn) or resets.
	function clearOurSelection() {
		const current = workspace.activeSelection;
		if (current !== null && current.path === path) {
			workspace.setActiveSelection(null);
		}
	}

	$effect(() => {
		const ec = editorConfigExtensions(workspace.editorConfigFor(path));
		const b = paneB;
		if (!b) {
			return;
		}
		merge?.a.dispatch({ effects: ecA.reconfigure(ec) });
		b.dispatch({ effects: ecB.reconfigure(ec) });
	});

	// Theme and soft-wrap follow the workspace toggles without a
	// rebuild; both panes flip together so the alignment holds.
	$effect(() => {
		const theme = workspace.effectiveTheme;
		const b = paneB;
		if (!b) {
			return;
		}
		merge?.a.dispatch({ effects: themeA.reconfigure(moonEditorTheme(theme)) });
		b.dispatch({ effects: themeB.reconfigure(moonEditorTheme(theme)) });
	});
	$effect(() => {
		const ext = workspace.lineWrap ? EditorView.lineWrapping : [];
		const b = paneB;
		if (!b) {
			return;
		}
		merge?.a.dispatch({ effects: wrapA.reconfigure(ext) });
		b.dispatch({ effects: wrapB.reconfigure(ext) });
	});

	onMount(() => {
		registerSection(path, sectionEl ?? null);
		registerFind?.(path, showFind);
		// A frame later, not inside the callback: crossing the narrow
		// threshold rebuilds the editors, and resizing observed
		// elements from their own callback is a "ResizeObserver loop"
		// error.
		let frame = -1;
		const resize = new ResizeObserver(() => {
			cancelAnimationFrame(frame);
			frame = requestAnimationFrame(() => {
				// A collapsed section's body has no box; its last real
				// width still decides the layout it reopens in.
				const width = host?.clientWidth ?? 0;
				if (width > 0) {
					hostWidth = width;
				}
			});
		});
		if (host) {
			hostWidth = host.clientWidth;
			resize.observe(host);
		}
		let io: IntersectionObserver | null = null;
		if (eager) {
			inView = true;
		} else if (sectionEl) {
			io = new IntersectionObserver(
				(entries) => {
					if (entries.some((entry) => entry.isIntersecting)) {
						io?.disconnect();
						inView = true;
					}
				},
				{ rootMargin: '50% 0px' },
			);
			io.observe(sectionEl);
		}
		return () => {
			io?.disconnect();
			resize.disconnect();
			cancelAnimationFrame(frame);
			registerSection(path, null);
			registerFind?.(path, null);
			buildToken++;
			destroyViews();
			clearOurSelection();
		};
	});
</script>

<section bind:this={sectionEl} class="commit-section" class:collapsed aria-label={`Diff of ${path}`}>
	<header class="hdr">
		{#if outline !== undefined}
			<button
				type="button"
				class="fold"
				aria-expanded={!collapsed}
				title={collapsed ? 'Show the diff' : 'Hide the diff'}
				onclick={outline.onToggle}
			>
				{#if collapsed}<ChevronRightIcon size={14} />{:else}<ChevronDownIcon size={14} />{/if}
			</button>
		{/if}
		<FileIcon {path} />
		<button type="button" class="path" title={`Open ${path} (${status})`} onclick={() => void openInEditor()}>
			{#if dirName(path)}<span class="dir">{dirName(path)}/</span>{/if}<span
				class="name"
				class:gone={status === 'deleted'}
				style:color={statusColor(status)}>{fileName(path)}</span
			>
		</button>
		<StatusWord {status} />
		<span class="grow"></span>
		{#if notes !== undefined}
			<button
				type="button"
				class="explain"
				title="Ask the agent what this file's changes do"
				onclick={() => notes?.onExplain(null)}
			>
				Explain
			</button>
		{/if}
		{#if outline !== undefined}
			{#if outline.attention > 0}
				<span class="attention" title="Unresolved threads and notes on this file">{outline.attention}</span>
			{/if}
			<DiffStat additions={outline.additions} deletions={outline.deletions} muted={outline.isTest} />
			<label class="viewed" title="Mark as viewed and move to the next file (v)">
				<input
					type="checkbox"
					checked={outline.viewed}
					onchange={(event) => outline.onViewed(event.currentTarget.checked)}
				/>
				Viewed
			</label>
		{/if}
	</header>
	<div class="body" bind:this={host}></div>
	{#if !collapsed && builtLayout === null}
		<div class="placeholder">{loading ? 'Loading diff…' : 'Scroll to load diff'}</div>
	{/if}
</section>

<style>
	.commit-section {
		display: flex;
		flex-direction: column;
		border: 1px solid var(--m-border);
		border-radius: 6px;
		background: var(--m-bg);
		/* `visible` (not `hidden`) so the sticky `.hdr` anchors to the
		 * outer `.commit-view` scroller. `hidden` makes this box a
		 * scroll container, which turns the header's `top: 35px` into
		 * an offset *inside the section* — permanently parking it over
		 * the first diff row (the top `… N unchanged lines`
		 * placeholder, which became invisible and unclickable) and
		 * never actually sticking. Same trap ReviewSection documents;
		 * `.body` below reapplies the clipping. */
		overflow: visible;
		scroll-margin-top: var(--m-review-banner-h, 12px);
	}
	.hdr {
		display: flex;
		align-items: center;
		gap: 8px;
		padding: 6px 10px;
		background: var(--m-bg-1);
		border-bottom: 1px solid var(--m-border);
		position: sticky;
		top: var(--m-review-banner-h, 0);
		z-index: 2;
		/* Round the top corners ourselves now that the section no
		 * longer clips its children. */
		border-radius: 5px 5px 0 0;
	}
	.name.gone {
		text-decoration: line-through;
	}
	.grow {
		flex: 1;
	}
	.explain {
		flex-shrink: 0;
		padding: 1px 6px;
		border: 1px solid transparent;
		border-radius: var(--m-radius-sm);
		background: transparent;
		color: var(--m-accent);
		font: inherit;
		font-size: var(--m-text-xs);
		cursor: pointer;
	}
	.explain:hover {
		border-color: var(--m-border-strong);
		background: var(--m-wash-hover);
	}
	.path {
		flex: 0 1 auto;
		min-width: 0;
		background: transparent;
		border: none;
		color: var(--m-fg-muted);
		font: inherit;
		font-size: 12px;
		text-align: left;
		cursor: pointer;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.path:hover {
		color: var(--m-fg);
	}
	.dir {
		color: var(--m-fg-subtle);
	}
	.commit-section.collapsed .hdr {
		border-bottom: none;
		border-radius: 5px;
	}
	.fold {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		flex-shrink: 0;
		width: 20px;
		height: 20px;
		margin-left: -4px;
		padding: 0;
		border: none;
		border-radius: var(--m-radius-sm);
		background: transparent;
		color: var(--m-fg-muted);
		cursor: pointer;
	}
	.fold:hover {
		background: var(--m-bg-3);
		color: var(--m-fg);
	}
	.attention {
		flex-shrink: 0;
		min-width: 16px;
		padding: 0 5px;
		border-radius: var(--m-radius-pill);
		background: color-mix(in srgb, var(--m-accent) 16%, transparent);
		color: var(--m-accent);
		font-size: var(--m-text-xs);
		font-variant-numeric: tabular-nums;
		line-height: 16px;
		text-align: center;
	}
	.viewed {
		display: inline-flex;
		align-items: center;
		gap: var(--m-space-1);
		flex-shrink: 0;
		margin-left: var(--m-space-2);
		color: var(--m-fg-muted);
		font-size: var(--m-text-xs);
		cursor: pointer;
	}
	.viewed input {
		margin: 0;
		accent-color: var(--m-accent);
		cursor: pointer;
	}
	.body {
		min-height: 0;
		/* Clips the MergeView's horizontal overflow and keeps the
		 * diff inside the card's rounded bottom corners — the
		 * containment the section box provided before it relaxed
		 * to `overflow: visible` for the sticky header. */
		overflow: hidden;
		border-radius: 0 0 5px 5px;
	}
	.placeholder {
		padding: 12px 10px;
		font-size: 11px;
		color: var(--m-fg-subtle);
	}
	/* Unified layout: one editor, grown to its content like the two
	 * panes of the split layout, so the stacked view keeps a single
	 * scrollbar. The theme asks for `height: 100%`. */
	.body > :global(.cm-editor) {
		height: auto;
	}
	.commit-section :global(.cm-editor.cm-focused) {
		outline: none;
	}
</style>
