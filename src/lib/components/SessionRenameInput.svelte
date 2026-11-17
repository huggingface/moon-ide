<script lang="ts">
	// Inline session rename. Enter or blur commits, Escape cancels;
	// either way `onDone` tells the owner to swap the title back in.
	import { untrack } from 'svelte';
	import { coder } from '../coder.svelte';
	import { formatError } from '../protocol';
	import { workspace } from '../state.svelte';

	type Props = {
		id: string;
		title: string;
		onDone: () => void;
	};
	let { id, title, onDone }: Props = $props();

	let text = $state(untrack(() => title));
	// Enter and Escape both unmount the input, and some webviews then
	// fire a `blur` that would otherwise commit a second time (or
	// commit what Escape just cancelled).
	let settled = false;

	async function commit(): Promise<void> {
		if (settled) {
			return;
		}
		settled = true;
		onDone();
		const next = text.trim();
		if (next.length === 0) {
			return;
		}
		try {
			await coder.renameSession(id, next);
		} catch (err) {
			workspace.flash(`Could not rename session: ${formatError(err)}`);
		}
	}

	function onKeydown(event: KeyboardEvent): void {
		if (event.key === 'Enter') {
			event.preventDefault();
			void commit();
			return;
		}
		if (event.key === 'Escape') {
			event.preventDefault();
			event.stopPropagation();
			settled = true;
			onDone();
		}
	}

	function focusAndSelect(node: HTMLInputElement): void {
		node.focus();
		node.select();
	}
</script>

<input
	class="session-rename"
	type="text"
	aria-label="Session title"
	bind:value={text}
	{@attach focusAndSelect}
	onclick={(event) => event.stopPropagation()}
	onkeydown={onKeydown}
	onblur={() => void commit()}
/>

<style>
	.session-rename {
		flex: 1 1 auto;
		min-width: 0;
		font: inherit;
		color: var(--m-fg);
		background: var(--m-bg);
		border: 1px solid var(--m-accent);
		border-radius: var(--m-radius-sm);
		padding: 0 4px;
		outline: none;
	}
</style>
