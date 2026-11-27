<script lang="ts">
	import { Progress } from '#lib/components/ui/progress/index.js';
	import { int } from '#lib/format.js';
	import { live } from '#lib/live.svelte.js';
	const sync = $derived(live.wallet.sync);
	const target = $derived(Math.max(sync?.network_height ?? 0, sync?.node_height ?? 0));
	const pct = $derived(target ? Math.min(100, ((sync?.wallet_height ?? 0) / target) * 100) : 0);
	const done = $derived(!!sync && target > 0 && sync.wallet_height + 1 >= target);
</script>

<div class="flex flex-col gap-1.5">
	<div class="flex justify-between font-mono text-xs">
		<span class="text-muted-foreground">
			{#if !sync?.daemon_online}node offline{:else if done}wallet synced{:else}scanning blocks{/if}
		</span>
		<span>{int(sync?.wallet_height)} / {int(target)}</span>
	</div>
	<Progress value={pct} max={100} class="h-1.5" />
</div>
