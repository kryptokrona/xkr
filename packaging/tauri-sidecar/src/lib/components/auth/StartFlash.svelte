<!-- Aesir's one-off light sweep when the login screen appears. -->
<div class="shine" aria-hidden="true"></div>

<style>
	.shine {
		position: absolute;
		inset: 0;
		height: 200%;
		z-index: 2;
		pointer-events: none;
		transform: translateY(100%);
		animation: slide 1000ms ease-in-out forwards;
		background-image: linear-gradient(
			180deg,
			rgb(255 255 255 / 0%) 1%,
			rgb(255 255 255 / 14%) 9%,
			rgb(255 255 255 / 0%) 100%
		);
	}
	@keyframes slide {
		to {
			transform: translateY(-100%);
		}
	}
	@media (prefers-reduced-motion: reduce) {
		.shine {
			display: none;
		}
	}
</style>
