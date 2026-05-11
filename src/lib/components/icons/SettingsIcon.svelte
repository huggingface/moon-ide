<script lang="ts">
	// 8-tooth cog. Used by the coder header's "Model settings"
	// affordance — the popover behind the icon lets the user pick
	// standard / cheap models, lock a default provider, and choose
	// the `X-HF-Bill-To` org.

	type Props = { size?: number };
	let { size = 14 }: Props = $props();
</script>

<svg
	xmlns="http://www.w3.org/2000/svg"
	width={size}
	height={size}
	viewBox="0 0 16 16"
	aria-hidden="true"
	focusable="false"
>
	<path
		d="M8 1.6v1.7M8 12.7v1.7M3.1 3.1l1.2 1.2M11.7 11.7l1.2 1.2M1.6 8h1.7M12.7 8h1.7M3.1 12.9l1.2-1.2M11.7 4.3l1.2-1.2"
		fill="none"
		stroke="currentColor"
		stroke-width="1.4"
		stroke-linecap="round"
	/>
	<circle cx="8" cy="8" r="3" fill="none" stroke="currentColor" stroke-width="1.4" />
</svg>
