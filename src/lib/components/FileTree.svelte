<script lang="ts">
	import { onMount, untrack } from 'svelte';
	import { FileTree } from '@pierre/trees';
	import { workspace } from '../state.svelte';

	let mount: HTMLDivElement;
	let tree: FileTree | undefined;

	onMount(() => {
		if (!mount) {
			return;
		}
		tree = new FileTree({
			paths: untrack(() => workspace.paths),
			flattenEmptyDirectories: true,
			initialExpansion: 1,
			search: true,
			onSelectionChange: (selectedPaths) => {
				if (selectedPaths.length === 0) {
					return;
				}
				const path = selectedPaths[0];
				if (path === undefined) {
					return;
				}
				const item = tree?.getItem(path);
				if (!item || item.isDirectory()) {
					return;
				}
				void workspace.openFile(path);
			},
		});
		tree.render({ containerWrapper: mount });
		return () => {
			tree?.cleanUp();
			tree = undefined;
		};
	});

	// Reactively reset paths when the workspace path list changes.
	$effect(() => {
		const paths = workspace.paths;
		if (!tree) {
			return;
		}
		tree.resetPaths(paths);
	});

	// Mirror the active file in the tree's selection so the row stays
	// highlighted as the user switches tabs (or restores a session). Two
	// invariants:
	//   1. If a file is active, exactly that file's row is selected.
	//   2. If no file is active (closed last tab, fresh workspace), the
	//      selection is cleared — leaving a stale row selected makes
	//      re-clicking the same row a no-op (Pierre only fires
	//      `onSelectionChange` on real changes).
	// We early-return when already in sync so a user click (which already
	// produces the desired selection via Pierre) doesn't trigger a
	// feedback loop through programmatic `select()`.
	$effect(() => {
		const target = workspace.activePath;
		if (!tree) {
			return;
		}
		const current = tree.getSelectedPaths();
		const alreadyInSync = target === null ? current.length === 0 : current.length === 1 && current[0] === target;
		if (alreadyInSync) {
			return;
		}
		for (const sel of current) {
			if (sel !== target) {
				tree.getItem(sel)?.deselect();
			}
		}
		if (target !== null) {
			tree.getItem(target)?.select();
		}
	});
</script>

<div class="tree" bind:this={mount}></div>

<style>
	.tree {
		height: 100%;
		width: 100%;
		overflow: hidden;
		--trees-row-font-family: var(--m-font-ui);
	}
</style>
