<script lang="ts">
	import { onMount } from 'svelte';
	import { FileDiff, type FileContents } from '@pierre/diffs';
	import { ipc } from '../ipc';
	import { workspace, type OpenFile } from '../state.svelte';

	type Props = { file: OpenFile };
	let { file }: Props = $props();

	let host: HTMLDivElement;
	// `$state` so the render effect below picks up the assignment
	// in `onMount` — a plain `let` isn't tracked by effects, so the
	// effect would see `undefined` forever and never call `render`.
	let instance = $state<FileDiff<undefined> | undefined>();
	// HEAD content for the "before" side. For a deleted buffer
	// `file.text` already holds it (see `loadDeletedFile` in
	// state.svelte.ts); for a modified buffer we fetch on mount /
	// path-change. `null` means "still loading" and suppresses
	// rendering to avoid a one-frame flash of empty-vs-empty.
	let headText = $state<string | null>(null);
	// Echoed so a stale fetch can't overwrite a newer one. We
	// capture the path the fetch was kicked off for and only accept
	// the result if it matches the current buffer — otherwise a
	// fast tab-switch during a slow `git show` would paint the
	// wrong diff.
	let headPath = $state<string | null>(null);

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

	// Fetch the HEAD "before" text whenever the bound path changes.
	// Deleted buffers bypass the IPC entirely: `file.text` is the
	// HEAD content, captured at open time. We don't re-fetch on
	// every `file.text` mutation because an editor-side keystroke
	// shouldn't re-roundtrip git — the DiffView won't be mounted
	// for a buffer the user is editing anyway.
	$effect(() => {
		const path = file.path;
		const deletedText = file.isDeleted ? file.text : null;
		if (deletedText !== null) {
			headPath = path;
			headText = deletedText;
			return;
		}
		headPath = null;
		headText = null;
		void (async () => {
			const fetched = await ipc.fs.gitHeadContent(path);
			if (file.path !== path) {
				return;
			}
			headPath = path;
			headText = fetched ?? '';
		})();
	});

	// Render (and re-render) whenever any of the inputs change.
	// `render` is Pierre's "accept this new state" call — internal
	// diffing re-runs with the new file pair. Guarded on
	// `headText !== null` so the first paint waits for HEAD.
	$effect(() => {
		const inst = instance;
		if (!inst || !host || headText === null || headPath !== file.path) {
			return;
		}
		const oldFile: FileContents = {
			name: file.name,
			contents: headText,
		};
		const newFile: FileContents = {
			name: file.name,
			// Deleted: the "after" side is intentionally empty.
			// Pierre renders the whole HEAD content as one big
			// deletion block — the exact "this file was removed"
			// shape the user's asking for.
			contents: file.isDeleted ? '' : file.text,
		};
		inst.render({ oldFile, newFile, containerWrapper: host });
	});

	// Flip Pierre's theme without rebuilding the component when
	// the IDE's effective theme changes.
	$effect(() => {
		const themeType = workspace.effectiveTheme;
		instance?.setThemeType(themeType);
	});
</script>

<div class="diff-view">
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
