<script lang="ts">
	import type { WalletTx } from '#lib/api.js';
	import { xkr, date, short } from '#lib/format.js';
	import ArrowDownLeft from '@lucide/svelte/icons/arrow-down-left';
	import ArrowUpRight from '@lucide/svelte/icons/arrow-up-right';
	import Pickaxe from '@lucide/svelte/icons/pickaxe';
	import Copy from '../Copy.svelte';
	let { tx, details = false }: { tx: WalletTx; details?: boolean } = $props();
	const incoming = $derived(tx.amount > 0);
</script>

<div
	class="border-border hover:bg-accent/50 flex items-center gap-4 border-b px-4 py-3 last:border-b-0
		{tx.confirmed ? '' : 'text-warning blink'}"
>
	<div
		class="flex size-8 shrink-0 items-center justify-center rounded-full
			{incoming ? 'bg-primary/15 text-primary' : 'bg-muted text-muted-foreground'}"
	>
		{#if tx.coinbase}<Pickaxe class="size-4" />{:else if incoming}<ArrowDownLeft class="size-4" />{:else}<ArrowUpRight class="size-4" />{/if}
	</div>
	<div class="flex min-w-0 flex-1 flex-col">
		<div class="flex items-center gap-1">
			<span class="selectable truncate font-mono text-sm">{details ? tx.hash : short(tx.hash, 10)}</span>
			{#if details}<Copy text={tx.hash} label="Transaction hash copied" />{/if}
		</div>
		<span class="text-muted-foreground font-mono text-xs">
			{tx.confirmed ? `${date(tx.timestamp)} · block ${tx.height.toLocaleString('en-US')}` : 'waiting for a block'}
			{#if details && tx.fee}· fee {xkr(tx.fee)}{/if}
			{#if details && tx.payment_id}· payment id {short(tx.payment_id, 6)}{/if}
		</span>
	</div>
	<span class="font-mono text-sm whitespace-nowrap {incoming ? 'text-primary' : ''}">
		{incoming ? '+' : ''}{xkr(tx.amount)} XKR
	</span>
</div>
