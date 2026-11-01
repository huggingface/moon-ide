<script lang="ts">
	import { workspace } from '../state.svelte';
	import BrowserFrame from './BrowserFrame.svelte';

	// Every browser tab's iframe, whichever folder or pane it belongs to
	// (ADR 0105).
	const tabs = $derived(Object.values(workspace.browserTabs));
</script>

{#each tabs as tab (tab.id)}
	<BrowserFrame {tab} />
{/each}
