<script lang="ts">
	import { formatBytes, percent, transfers, type TransferRow } from '../transfers.svelte';

	function detail(row: TransferRow): string {
		if (row.state === 'failed') {
			return row.error ?? 'Failed';
		}
		if (row.state === 'cancelled') {
			return 'Cancelled';
		}
		if (row.state === 'done') {
			return `Done · ${formatBytes(row.total)}`;
		}
		if (row.total === 0) {
			return formatBytes(row.bytes);
		}
		return `${percent(row.bytes, row.total)}% · ${formatBytes(row.bytes)} of ${formatBytes(row.total)}`;
	}
</script>

{#if transfers.rows.length > 0}
	<div class="transfers" role="status">
		{#each transfers.rows as row (row.id)}
			<div class="row" class:failed={row.state === 'failed'}>
				<div class="head">
					<span class="name" title={row.name}>
						{row.direction === 'upload' ? 'Uploading' : 'Downloading'}
						{row.name}
					</span>
					{#if row.state === 'running'}
						<button type="button" class="action" onclick={() => transfers.cancel(row.id)}>Cancel</button>
					{:else if row.state === 'failed'}
						<button type="button" class="action" onclick={() => transfers.dismiss(row.id)}>Dismiss</button>
					{/if}
				</div>
				<div class="detail" title={detail(row)}>{detail(row)}</div>
				{#if row.state === 'running'}
					<div class="bar"><div class="fill" style:width="{percent(row.bytes, row.total)}%"></div></div>
				{/if}
			</div>
		{/each}
	</div>
{/if}

<style>
	.transfers {
		position: fixed;
		bottom: 36px;
		right: 16px;
		display: flex;
		flex-direction: column;
		gap: 8px;
		width: 340px;
		z-index: 20;
	}
	.row {
		background: var(--m-bg-2);
		border: 1px solid var(--m-border-strong);
		color: var(--m-fg);
		padding: 8px 12px;
		border-radius: 6px;
		box-shadow: 0 6px 24px rgba(0, 0, 0, 0.5);
		font-size: 12px;
	}
	.row.failed {
		border-color: var(--m-danger);
	}
	.head {
		display: flex;
		align-items: center;
		gap: 8px;
	}
	.name {
		flex: 1;
		min-width: 0;
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}
	.action {
		background: none;
		border: 1px solid var(--m-border-strong);
		color: var(--m-fg-muted);
		border-radius: 4px;
		padding: 1px 8px;
		font-size: 11px;
		cursor: pointer;
	}
	.action:hover {
		color: var(--m-fg);
	}
	.detail {
		margin-top: 4px;
		color: var(--m-fg-muted);
		font-variant-numeric: tabular-nums;
		overflow-wrap: anywhere;
	}
	.failed .detail {
		color: var(--m-danger);
	}
	.bar {
		margin-top: 6px;
		height: 3px;
		background: var(--m-bg-3);
		border-radius: 2px;
		overflow: hidden;
	}
	.fill {
		height: 100%;
		background: var(--m-accent);
	}
</style>
