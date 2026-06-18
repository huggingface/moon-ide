<script lang="ts">
	// Advanced, deliberately tucked-away affordance on `write_file`
	// / `edit_file` tool rows: reapply the recorded edit to disk.
	// The recovery hatch for "I reset / clobbered that file and want
	// the agent's change back" without re-running the whole turn.
	//
	// Rendered only inside the expanded tool body (you have to open
	// the row first), and gated behind a one-item menu rather than a
	// bare button — `write_file` overwrites the whole file, so the
	// extra click guards against an accidental clobber. Pure
	// side-effect: the backend dispatches the call again and touches
	// disk, but never the transcript or the JSONL.
	import ContextMenu from './ContextMenu.svelte';
	import type { ContextMenuItem } from './contextMenu';
	import { ipc } from '../ipc';
	import { formatError } from '../protocol';
	import { workspace } from '../state.svelte';

	interface Props {
		/** Tool-call id the backend looks the recorded args up by. */
		callId: string;
	}

	let { callId }: Props = $props();

	let anchorRect = $state<{ left: number; top: number; width: number; height: number } | null>(null);
	let busy = $state(false);

	function openMenu(event: MouseEvent): void {
		const rect = (event.currentTarget as HTMLElement).getBoundingClientRect();
		anchorRect = { left: rect.left, top: rect.top, width: rect.width, height: rect.height };
	}

	async function reapply(): Promise<void> {
		if (busy) {
			return;
		}
		busy = true;
		try {
			const outcome = await ipc.coder.rerunToolCall(callId);
			workspace.flash(`Re-applied ${outcome.tool_name} to disk.`);
		} catch (err) {
			workspace.flash(`Re-apply failed: ${formatError(err)}`);
		} finally {
			busy = false;
		}
	}

	const items: ContextMenuItem[] = $derived([
		{
			id: 'reapply',
			label: 'Re-apply this edit to disk',
			title: 'Dispatch this edit again against the current file',
			disabled: busy,
			onSelect: () => void reapply(),
		},
	]);
</script>

<button
	type="button"
	class="reapply-cog"
	title="Re-apply this edit to disk"
	aria-label="Re-apply this edit to disk"
	disabled={busy}
	onclick={openMenu}
>
	<svg
		xmlns="http://www.w3.org/2000/svg"
		width="13"
		height="13"
		viewBox="0 0 16 16"
		fill="none"
		stroke="currentColor"
		stroke-width="1.5"
		stroke-linecap="round"
		stroke-linejoin="round"
		aria-hidden="true"
		focusable="false"
	>
		<path d="M14 8a6 6 0 1 1-1.76-4.24" />
		<path d="M14 2v4h-4" />
	</svg>
</button>
{#if anchorRect !== null}
	<ContextMenu {items} {anchorRect} onClose={() => (anchorRect = null)} />
{/if}

<style>
	.reapply-cog {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		flex: 0 0 auto;
		padding: 2px;
		color: var(--m-fg-subtle);
		background: transparent;
		border: none;
		border-radius: 4px;
		cursor: pointer;
		opacity: 0.6;
	}
	.reapply-cog:hover:not(:disabled) {
		color: var(--m-fg);
		background: var(--m-bg-hover, color-mix(in srgb, var(--m-fg) 8%, transparent));
		opacity: 1;
	}
	.reapply-cog:disabled {
		cursor: default;
		opacity: 0.35;
	}
</style>
