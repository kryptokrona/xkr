<!-- The 25 seed words in a numbered grid. -->
<script lang="ts">
	import Copy from '../Copy.svelte';
	let { mnemonic }: { mnemonic: string } = $props();
	const words = $derived(mnemonic.split(/\s+/).filter(Boolean));
</script>

<div class="flex flex-col gap-2">
	<ol class="border-border grid grid-cols-5 gap-2 rounded-xl border p-3">
		{#each words as w, i}
			<li class="bg-muted selectable rounded-md px-2 py-1.5 font-mono text-xs">
				<span class="text-muted-foreground mr-1">{i + 1}</span>{w}
			</li>
		{/each}
	</ol>
	<div class="flex items-center justify-end gap-1 text-xs">
		<span class="text-muted-foreground">Copy all words</span>
		<Copy text={mnemonic} label="Seed words copied" />
	</div>
</div>
