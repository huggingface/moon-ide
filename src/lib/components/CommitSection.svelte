<script lang="ts">
	import { onMount } from 'svelte';
	import { Compartment, EditorState, type Extension } from '@codemirror/state';
	import { EditorView, lineNumbers } from '@codemirror/view';
	import { foldGutter } from '@codemirror/language';
	import { highlightSelectionMatches } from '@codemirror/search';
	import { searchAsYouType } from '../editor/searchAsYouType';
	import { MergeView, diff as rawDiff } from '@codemirror/merge';
	import { ipc } from '../ipc';
	import { workspace, type SplitSide } from '../state.svelte';
	import { highlightTabs } from '../editor/highlightTabs';
	import { languageFor } from '../editor/language';
	import { moonEditorTheme } from '../editor/theme';
	import { diffPureChangeExtension } from '../editor/diffPureChange';
	import { diffGutterTintExtension } from '../editor/diffGutterTint';
	import { diffCollapseContextExtension } from '../editor/diffCollapseContext';
	import type { GitFileStatus } from '../protocol';

	type Props = {
		path: string;
		status: GitFileStatus;
		// Full SHAs: the commit and its first parent. Both sides of
		// the MergeView read from `git_ref_content` — the base side
		// from `parentSha`, the after side from `commitSha`. An empty
		// `parentSha` (root commit) renders the base side empty.
		commitSha: string;
		parentSha: string;
		eager: boolean;
		registerSection: (path: string, el: HTMLElement | null) => void;
		side: SplitSide;
	};

	let { path, status, commitSha, parentSha, eager, registerSection, side }: Props = $props();

	let sectionEl: HTMLElement | undefined = $state();
	let host: HTMLDivElement | undefined = $state();
	let merge: MergeView | undefined = $state();
	let mounted = $state(false);
	let loading = $state(false);
	let buildToken = 0;
	let detachHScrollSync: (() => void) | null = null;

	const langA = new Compartment();
	const langB = new Compartment();
	const themeA = new Compartment();
	const themeB = new Compartment();
	const wrapA = new Compartment();
	const wrapB = new Compartment();

	function fileName(p: string): string {
		const slash = p.lastIndexOf('/');
		return slash === -1 ? p : p.slice(slash + 1);
	}
	function dirName(p: string): string {
		const slash = p.lastIndexOf('/');
		return slash === -1 ? '' : p.slice(0, slash);
	}

	function statusLabel(s: GitFileStatus): string {
		switch (s) {
			case 'added':
				return 'A';
			case 'modified':
				return 'M';
			case 'deleted':
				return 'D';
			case 'conflicted':
				return '!';
			default:
				return s[0]?.toUpperCase() ?? '?';
		}
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
			const content = await ipc.fs.gitRefContent(rev, path);
			return content ?? '';
		} catch {
			return '';
		}
	}

	async function build() {
		if (mounted || loading || !host) {
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
		const lang = await languageFor(path, firstLine);
		if (token !== buildToken || !host) {
			return;
		}

		const readOnly: Extension[] = [EditorState.readOnly.of(true), EditorView.editable.of(false)];

		const commonExts: Extension[] = [
			lineNumbers(),
			diffGutterTintExtension('a'),
			diffCollapseContextExtension,
			foldGutter(),
			diffPureChangeExtension,
			highlightSelectionMatches(),
			searchAsYouType(),
			highlightTabs(),
		];

		const sideA: Extension[] = [
			...commonExts,
			themeA.of(moonEditorTheme(workspace.effectiveTheme)),
			langA.of(lang),
			wrapA.of(workspace.lineWrap ? EditorView.lineWrapping : []),
			...readOnly,
		];

		const sideB: Extension[] = [
			lineNumbers(),
			diffGutterTintExtension('b'),
			diffCollapseContextExtension,
			foldGutter(),
			diffPureChangeExtension,
			highlightSelectionMatches(),
			searchAsYouType(),
			highlightTabs(),
			themeB.of(moonEditorTheme(workspace.effectiveTheme)),
			langB.of(lang),
			wrapB.of(workspace.lineWrap ? EditorView.lineWrapping : []),
			...readOnly,
		];

		detachHScrollSync?.();
		detachHScrollSync = null;

		merge = new MergeView({
			a: { doc: base, extensions: sideA },
			b: { doc: after, extensions: sideB },
			parent: host,
			gutter: false,
			highlightChanges: true,
			collapseUnchanged: { margin: 3, minSize: 5 },
			diffConfig: { override: rawDiff },
		});

		detachHScrollSync = wireHorizontalScrollSync(merge.a.scrollDOM, merge.b.scrollDOM);
		mounted = true;
		loading = false;
	}

	function wireHorizontalScrollSync(a: HTMLElement, b: HTMLElement): () => void {
		const expected = new WeakMap<HTMLElement, number>();
		const handle = (from: HTMLElement, to: HTMLElement) => {
			const pending = expected.get(from);
			if (pending !== undefined && from.scrollLeft === pending) {
				expected.delete(from);
				return;
			}
			expected.delete(from);
			const toMax = Math.max(0, to.scrollWidth - to.clientWidth);
			const target = Math.min(from.scrollLeft, toMax);
			if (to.scrollLeft === target) {
				return;
			}
			expected.set(to, target);
			to.scrollLeft = target;
		};
		const onA = () => handle(a, b);
		const onB = () => handle(b, a);
		a.addEventListener('scroll', onA, { passive: true });
		b.addEventListener('scroll', onB, { passive: true });
		return () => {
			a.removeEventListener('scroll', onA);
			b.removeEventListener('scroll', onB);
		};
	}

	function openInEditor() {
		void workspace.openFile(path, side);
	}

	onMount(() => {
		registerSection(path, sectionEl ?? null);
		if (eager) {
			void build();
		} else if (sectionEl) {
			const io = new IntersectionObserver(
				(entries) => {
					for (const entry of entries) {
						if (entry.isIntersecting) {
							io.disconnect();
							void build();
							return;
						}
					}
				},
				{ rootMargin: '50% 0px' },
			);
			io.observe(sectionEl);
			return () => {
				io.disconnect();
				registerSection(path, null);
				buildToken++;
				detachHScrollSync?.();
				detachHScrollSync = null;
				merge?.destroy();
				merge = undefined;
			};
		}
		return () => {
			registerSection(path, null);
			buildToken++;
			detachHScrollSync?.();
			detachHScrollSync = null;
			merge?.destroy();
			merge = undefined;
		};
	});
</script>

<section bind:this={sectionEl} class="commit-section" aria-label={`Diff of ${path}`}>
	<header class="hdr">
		<span class="status status-{status}" title={`Status: ${status}`} aria-label={`Status ${status}`}>
			{statusLabel(status)}
		</span>
		<button type="button" class="path" title={`Open ${path}`} onclick={openInEditor}>
			{#if dirName(path)}<span class="dir">{dirName(path)}/</span>{/if}<span class="name">{fileName(path)}</span>
		</button>
	</header>
	<div class="body" bind:this={host}></div>
	{#if !mounted && loading}
		<div class="placeholder">Loading diff…</div>
	{:else if !mounted}
		<div class="placeholder">Scroll to load diff</div>
	{/if}
</section>

<style>
	.commit-section {
		display: flex;
		flex-direction: column;
		border: 1px solid var(--m-border);
		border-radius: 6px;
		background: var(--m-bg);
		overflow: hidden;
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
	}
	.status {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		min-width: 16px;
		height: 16px;
		padding: 0 4px;
		font-size: 10px;
		font-weight: 600;
		border-radius: 3px;
		font-family: var(--m-font-mono, monospace);
	}
	.status-added {
		color: var(--m-green);
		background: color-mix(in srgb, var(--m-green) 14%, transparent);
	}
	.status-modified {
		color: var(--m-blue);
		background: color-mix(in srgb, var(--m-blue) 14%, transparent);
	}
	.status-deleted {
		color: var(--m-red);
		background: color-mix(in srgb, var(--m-red) 14%, transparent);
	}
	.status-conflicted {
		color: var(--m-warning);
		background: color-mix(in srgb, var(--m-warning) 14%, transparent);
	}
	.path {
		flex: 1;
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
	.body {
		min-height: 0;
	}
	.placeholder {
		padding: 12px 10px;
		font-size: 11px;
		color: var(--m-fg-subtle);
	}
	/* Character-level change marker — same rules as
	 * `ReviewSection.svelte` and `DiffView.svelte`: the library
	 * default is a 2px bottom-edge gradient that reads as a loud
	 * underline. Swap for a soft same-hue background (GitHub-style
	 * inline diff highlight) using our palette tokens so theme flips
	 * track. `!important` beats the package's themed rules. */
	.commit-section :global(.cm-merge-b .cm-changedText) {
		background: color-mix(in srgb, var(--m-success) 22%, transparent) !important;
		border-radius: 2px;
	}
	.commit-section :global(.cm-merge-a .cm-changedText),
	.commit-section :global(.cm-deletedChunk .cm-deletedText) {
		background: color-mix(in srgb, var(--m-danger) 22%, transparent) !important;
		border-radius: 2px;
	}
	.commit-section :global(.cm-merge-b .cm-deletedText) {
		background: color-mix(in srgb, var(--m-danger) 22%, transparent) !important;
	}
	/* Pure-added / pure-deleted lines: the gutter tint already
	 * conveys the line-level change, so clear the per-character
	 * marker to avoid doubling up the same hue. */
	.commit-section :global(.cm-moon-pure-change .cm-changedText) {
		background: transparent !important;
	}
</style>
