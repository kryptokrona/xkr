<!-- The Kryptokrona mark (as in the Aesir wallet): stripes and a bar in a circle. -->
<script lang="ts">
	let { size = 24, class: className = '' }: { size?: number; class?: string } = $props();
	const stripes = [-21, 83.43, 187.86, 292.29, 396.72];
	const id = `xkr-logo-${Math.random().toString(36).slice(2)}`;
</script>

<svg width={size} height={size} viewBox="0 0 458 458" class={className} aria-hidden="true">
	<defs>
		<clipPath {id}><circle cx="229" cy="229" r="229" /></clipPath>
	</defs>
	<g clip-path="url(#{id})" fill="currentColor">
		<rect x="187.65" y="0" width="81.7" height="458" />
		{#each stripes as y}
			<rect x="295" {y} width="200" height="81.69" />
			<rect x="-40" {y} width="203" height="81.69" />
		{/each}
	</g>
</svg>
