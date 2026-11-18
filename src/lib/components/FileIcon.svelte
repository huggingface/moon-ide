<script lang="ts">
	// The file tree's icon for a file's kind, in that kind's colour.
	import { fileIconFor, mountFileIconSprite } from '../fileIcon';

	type Props = { path: string; size?: number };
	let { path, size = 15 }: Props = $props();

	const icon = $derived(fileIconFor(path));
	mountFileIconSprite();
</script>

<svg
	class="file-icon"
	width={size}
	height={size}
	viewBox="0 0 16 16"
	style:color={icon.color}
	aria-hidden="true"
	focusable="false"
>
	<use href={`#${icon.symbol}`} />
</svg>

<style>
	.file-icon {
		flex-shrink: 0;
	}
</style>
