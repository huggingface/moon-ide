<script lang="ts">
	// A turning arc: "this is in progress". Spins on its own.

	type Props = { size?: number };
	let { size = 14 }: Props = $props();
</script>

<svg
	class="spinner"
	xmlns="http://www.w3.org/2000/svg"
	width={size}
	height={size}
	viewBox="0 0 16 16"
	fill="none"
	stroke="currentColor"
	stroke-width="1.75"
	stroke-linecap="round"
	aria-hidden="true"
	focusable="false"
>
	<circle cx="8" cy="8" r="5.75" opacity="0.25" />
	<path d="M8 2.25a5.75 5.75 0 0 1 5.75 5.75" />
</svg>

<style>
	.spinner {
		flex-shrink: 0;
		animation: spinner-turn 0.8s linear infinite;
	}
	@keyframes spinner-turn {
		to {
			transform: rotate(360deg);
		}
	}
</style>
