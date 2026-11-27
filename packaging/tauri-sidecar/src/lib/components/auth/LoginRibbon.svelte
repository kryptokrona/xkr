<!-- Aesir's login backdrop: rows of drifting Kryptokrona wordmarks at an angle, seen through a
     soft round hole. Decorative only. -->
<script lang="ts">
	import { onMount } from 'svelte';
	import { blur } from 'svelte/transition';

	const angle = 40;
	const logoWidth = 128;
	const gapX = -10;
	const rowHeight = Math.round((logoWidth * 54) / 176);
	const pitch = rowHeight + 52;
	const cell = logoWidth + gapX;

	let cols = $state(30);
	let rows = $state(30);
	let mounted = $state(false);
	const half = $derived(cols * cell);

	const size = () => {
		const field = 4 * Math.max(window.innerWidth, window.innerHeight);
		cols = Math.ceil(field / cell) + 2;
		rows = Math.ceil(field / pitch) + 1;
	};

	onMount(() => {
		size();
		window.addEventListener('resize', size);
		mounted = true;
		return () => window.removeEventListener('resize', size);
	});

	const lines = $derived(
		Array.from({ length: rows }, (_, i) => ({
			opacity: 0.05 + 0.06 * (0.5 + 0.5 * Math.sin(-i * 0.7)),
			reverse: i % 2 === 1,
			duration: (120 + (i % 5) * 0.4) * (cell / 20)
		}))
	);
</script>

{#if mounted}
	<div class="ribbon" in:blur aria-hidden="true">
		<div class="field" style="--angle: {angle}deg; --pitch: {pitch}px; --half: {half}px;">
			{#each lines as line}
				<div class="row" style="--h: {rowHeight}px; --op: {line.opacity}; --dur: {line.duration}s;">
					<div class="track" class:reverse={line.reverse}>
						{#each Array(cols * 2) as _}
							<span class="logo" style="width: {logoWidth}px; margin-right: {gapX}px;"></span>
						{/each}
					</div>
				</div>
			{/each}
		</div>
	</div>
{/if}

<style>
	.ribbon {
		position: absolute;
		top: 50%;
		left: 50%;
		width: min(560px, 88%);
		height: min(520px, 76%);
		transform: translate(-50%, -50%);
		z-index: 0;
		overflow: hidden;
		pointer-events: none;
		border-radius: 15px;
		-webkit-mask: radial-gradient(circle at 50% 40%, #000 18%, transparent 42%);
		mask: radial-gradient(circle at 50% 40%, #000 18%, transparent 42%);
	}
	.field {
		position: absolute;
		top: 50%;
		left: 50%;
		width: 220vmax;
		height: 220vmax;
		transform: translate(-50%, -50%) rotate(calc(var(--angle) * -1));
		display: flex;
		flex-direction: column;
		justify-content: center;
		gap: calc(var(--pitch) - var(--h));
	}
	.row {
		height: var(--h);
		width: 100%;
		opacity: var(--op);
		overflow: hidden;
	}
	.track {
		display: flex;
		align-items: center;
		width: max-content;
		height: 100%;
		will-change: transform;
		animation: drift var(--dur) linear infinite;
	}
	.track.reverse {
		animation-name: drift-rev;
	}
	.logo {
		height: var(--h);
		flex: none;
		display: block;
		background-color: var(--foreground);
		-webkit-mask: url(/logo.svg) no-repeat center / 100% 100%;
		mask: url(/logo.svg) no-repeat center / 100% 100%;
	}
	@keyframes drift {
		to {
			transform: translateX(calc(var(--half) * -1));
		}
	}
	@keyframes drift-rev {
		from {
			transform: translateX(calc(var(--half) * -1));
		}
		to {
			transform: translateX(0);
		}
	}
	@media (prefers-reduced-motion: reduce) {
		.track {
			animation: none;
		}
	}
</style>
