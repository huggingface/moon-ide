<script lang="ts">
	import { onMount } from 'svelte';
	import { FileDiff, type FileContents } from '@pierre/diffs';
	import { ipc } from '../ipc';
	import { workspace, type OpenFile, type SplitSide } from '../state.svelte';

	function basename(path: string): string {
		const i = path.lastIndexOf('/');
		return i >= 0 ? path.slice(i + 1) : path;
	}

	type Props = { file: OpenFile; side: SplitSide };
	let { file, side }: Props = $props();

	// Wrapper element we keep focusable + programmatically focus.
	// Without this, opening a diff tab from the file tree leaves
	// focus on the tree row — which means Pierre's tree consumes
	// Alt+Arrow (not to mention regular arrow keys) and the user
	// sees the tree row light up as the "focused" thing rather
	// than the pane they just opened.
	let wrapper: HTMLDivElement;
	let host: HTMLDivElement;
	// `$state` so the render effect below picks up the assignment
	// in `onMount` — a plain `let` isn't tracked by effects, so the
	// effect would see `undefined` forever and never call `render`.
	let instance = $state<FileDiff<undefined> | undefined>();
	// HEAD content for the "before" side. For a deleted buffer
	// `file.text` already holds it (see `loadDeletedFile` in
	// state.svelte.ts); for a modified buffer or a dedicated diff
	// tab we fetch on mount / path-change. `null` means "still
	// loading" and suppresses rendering to avoid a one-frame flash
	// of empty-vs-empty.
	let headText = $state<string | null>(null);
	// Echoed so a stale fetch can't overwrite a newer one. Keyed on
	// the *real* path (not the synthetic `moon-diff:` tab id) so the
	// cache keys line up with `workspace.headByPath` and a slow
	// `git show` during a fast tab swap can't paint the wrong side.
	let headPath = $state<string | null>(null);
	// "After" side for diff tabs of modified files. Prefer the live
	// editor buffer if one exists — typing in the regular tab then
	// flipping to the diff tab should reflect unsaved edits — else
	// fall back to a one-shot `readFile` on mount.
	let diskAfterText = $state<string | null>(null);

	// Workspace-relative path this DiffView's "before/after" pair
	// is computed against. Deleted buffers and the (no-longer-used)
	// legacy in-place toggle key on `file.path` itself; a dedicated
	// diff tab keys on `file.realPath`.
	const targetPath = $derived(file.isDiffTab ? file.realPath : file.path);
	const targetName = $derived(file.isDiffTab ? basename(file.realPath) : file.name);

	// Prefer the open editor tab's text for the "after" side of a
	// diff tab. Matches users' mental model — the diff reflects the
	// buffer they'd hit `Ctrl+S` on, not a stale snapshot from when
	// the tab was opened.
	const liveAfterText = $derived.by(() => {
		if (!file.isDiffTab) {
			return null;
		}
		const editorTab = workspace.openFiles.find((f) => f.path === file.realPath && f.kind === 'text' && !f.isDiffTab);
		return editorTab ? editorTab.text : null;
	});

	onMount(() => {
		instance = new FileDiff({
			// Shiki themes: `pierre-light` / `pierre-dark` ship in the
			// package and mostly match the VSCode/Cursor theme family
			// the team uses. Swap the mapping when we grow a proper
			// Pierre-theme adoption in a later phase; for now a fixed
			// pair keeps light/dark readable and consistent with the
			// CodeMirror editor's colour palette.
			theme: { dark: 'pierre-dark', light: 'pierre-light' },
			themeType: workspace.effectiveTheme,
			diffStyle: 'split',
			diffIndicators: 'bars',
			// The editor already has its own toolbar header (tab +
			// pane chrome); Pierre's file header would be a second
			// filename banner immediately below it. Suppress it —
			// the user already knows which file they're looking at.
			disableFileHeader: true,
		});
		return () => {
			instance?.cleanUp();
			instance = undefined;
		};
	});

	// Fetch the HEAD "before" text whenever the bound real path
	// changes. Deleted buffers bypass the IPC entirely: `file.text`
	// is the HEAD content, captured at open time. Diff tabs and
	// modified-file toggles both route through `git show` here.
	$effect(() => {
		const path = targetPath;
		const deletedText = file.isDeleted ? file.text : null;
		if (deletedText !== null) {
			headPath = path;
			headText = deletedText;
			return;
		}
		// Reuse workspace-level cache when present — the gutter
		// extension keeps it warm for any open file, so a diff tab
		// opened right after the editor tab paints instantly.
		const cached = workspace.headByPath.get(path);
		if (cached !== undefined) {
			headPath = path;
			headText = cached ?? '';
			return;
		}
		headPath = null;
		headText = null;
		void (async () => {
			const fetched = await ipc.fs.gitHeadContent(path);
			if (targetPath !== path) {
				return;
			}
			headPath = path;
			headText = fetched ?? '';
		})();
	});

	// Fetch the on-disk "after" side for diff tabs whose `realPath`
	// isn't currently open in an editor tab. One-shot: the user can
	// close and reopen the diff tab to re-pull, or (more likely)
	// open the regular editor tab and watch it update live.
	$effect(() => {
		if (!file.isDiffTab || liveAfterText !== null) {
			diskAfterText = null;
			return;
		}
		const path = targetPath;
		diskAfterText = null;
		void (async () => {
			try {
				const result = await ipc.fs.readFile(path);
				if (targetPath !== path) {
					return;
				}
				// Binary paths will land here as `is_binary = true`
				// — for diff purposes we treat the working-tree
				// side as empty in that case, matching how
				// `git_head_content` handles binaries server-side.
				diskAfterText = result.is_binary ? '' : result.text;
			} catch {
				// Real path no longer exists on disk (externally
				// deleted after the diff tab opened). Empty string
				// paints the whole file as a deletion, which is
				// the honest answer.
				if (targetPath !== path) {
					return;
				}
				diskAfterText = '';
			}
		})();
	});

	// Render (and re-render) whenever any of the inputs change.
	// `render` is Pierre's "accept this new state" call — internal
	// diffing re-runs with the new file pair. Guarded on
	// `headText !== null` so the first paint waits for HEAD.
	$effect(() => {
		const inst = instance;
		if (!inst || !host || headText === null || headPath !== targetPath) {
			return;
		}
		// Pick the most authoritative "after" side we have. Order:
		// 1) Deleted buffer → empty (the file is gone; its HEAD
		//    side renders as one big deletion block).
		// 2) Diff tab with live editor buffer → live buffer.
		// 3) Diff tab without an open editor → on-disk snapshot.
		// 4) Neither — this path shouldn't be reachable now that
		//    the legacy in-place toggle is gone; guard with `''`.
		let afterContents: string;
		if (file.isDeleted) {
			afterContents = '';
		} else if (file.isDiffTab) {
			if (liveAfterText !== null) {
				afterContents = liveAfterText;
			} else if (diskAfterText !== null) {
				afterContents = diskAfterText;
			} else {
				return;
			}
		} else {
			afterContents = file.text;
		}
		const oldFile: FileContents = {
			name: targetName,
			contents: headText,
		};
		const newFile: FileContents = {
			name: targetName,
			contents: afterContents,
		};
		inst.render({ oldFile, newFile, containerWrapper: host });
	});

	// Flip Pierre's theme without rebuilding the component when
	// the IDE's effective theme changes.
	$effect(() => {
		const themeType = workspace.effectiveTheme;
		instance?.setThemeType(themeType);
	});

	// Pull focus into the diff pane whenever the workspace bumps
	// `focusTick` (mirrors `Editor.svelte`'s pattern). Microtask-
	// deferred so the click that triggered the bump — typically a
	// context-menu select in the file tree — finishes settling
	// its own focus first; without the defer the browser often
	// hands focus back to the original click target.
	$effect(() => {
		workspace.focusTick;
		if (workspace.focusedSide !== side) {
			return;
		}
		const el = wrapper;
		if (!el) {
			return;
		}
		queueMicrotask(() => el.focus({ preventScroll: true }));
	});
</script>

<!-- svelte-ignore a11y_no_noninteractive_tabindex -->
<div class="diff-view" tabindex="0" bind:this={wrapper}>
	<div class="diff-host" bind:this={host}></div>
</div>

<style>
	.diff-view {
		flex: 1;
		min-width: 0;
		min-height: 0;
		display: flex;
		flex-direction: column;
		background: var(--m-bg);
		color: var(--m-fg);
		overflow: auto;
	}
	.diff-host {
		flex: 1;
		min-width: 0;
		min-height: 0;
	}
</style>
