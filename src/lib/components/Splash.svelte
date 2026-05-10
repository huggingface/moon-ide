<script lang="ts">
	// Shown during the brief window between mount and the first
	// resolved-workspace paint. Prevents the Welcome screen from
	// flashing its `Add folder` card when there _is_ a persisted
	// folder — `App.svelte` hides this once `workspace.hydrated`
	// flips. Styled only with themed CSS vars so it inherits the
	// saved theme as soon as `restoreAppState` applies it (usually
	// one frame later).
</script>

<div class="splash" aria-hidden="true">
	<div class="wordmark">moon-ide</div>
	<div class="bar"><span class="bar-fill"></span></div>
</div>

<style>
	.splash {
		width: 100%;
		height: 100%;
		display: flex;
		flex-direction: column;
		align-items: center;
		justify-content: center;
		gap: 18px;
		background: var(--m-bg);
	}
	.wordmark {
		font-size: 22px;
		font-weight: 600;
		letter-spacing: -0.01em;
		color: var(--m-fg-muted);
	}
	.bar {
		width: 140px;
		height: 2px;
		background: var(--m-border);
		border-radius: 1px;
		overflow: hidden;
	}
	.bar-fill {
		display: block;
		width: 40%;
		height: 100%;
		background: var(--m-accent);
		animation: slide 1.2s ease-in-out infinite;
	}

	@keyframes slide {
		0% {
			transform: translateX(-100%);
		}
		50% {
			transform: translateX(200%);
		}
		100% {
			transform: translateX(350%);
		}
	}

	@media (prefers-reduced-motion: reduce) {
		.bar-fill {
			animation: none;
			width: 100%;
			opacity: 0.4;
		}
	}
</style>
