<script lang="ts">
	import { toast } from 'svelte-sonner';
	import RefreshCw from '@lucide/svelte/icons/refresh-cw';
	import * as Dialog from '#lib/components/ui/dialog/index.js';
	import { Button } from '#lib/components/ui/button/index.js';
	import { Input } from '#lib/components/ui/input/index.js';
	import { Label } from '#lib/components/ui/label/index.js';
	import Page from '#lib/components/Page.svelte';
	import WalletNav from '#lib/components/wallet/WalletNav.svelte';
	import TxRow from '#lib/components/wallet/TxRow.svelte';
	import { api, type WalletTx } from '#lib/api.js';
	import { live } from '#lib/live.svelte.js';

	const PAGE = 100;
	let limit = $state(PAGE);
	let txs = $state<WalletTx[]>([]);
	let total = $state(0);
	let rescanOpen = $state(false);
	let rescanHeight = $state('0');

	$effect(() => {
		void live.wallet.sync?.wallet_height;
		api.walletTransactions(limit).then((r) => {
			txs = r.transactions;
			total = r.total;
		}).catch(() => {});
	});

	async function rescan() {
		try {
			await api.walletRescan(Number(rescanHeight) || 0);
			toast.success('Scanning the chain again');
			rescanOpen = false;
		} catch (e) {
			toast.error(String(e));
		}
	}
</script>

<Page title="History" subtitle="{total} transactions">
	{#snippet actions()}
		<WalletNav />
		<Button variant="ghost" size="icon" aria-label="Rescan" onclick={() => (rescanOpen = true)}><RefreshCw /></Button>
	{/snippet}

	<div class="border-border overflow-hidden rounded-xl border">
		{#each txs as tx (tx.hash)}
			<TxRow {tx} details />
		{:else}
			<p class="text-muted-foreground px-4 py-10 text-center text-sm">No transactions yet.</p>
		{/each}
	</div>
	{#if txs.length < total}
		<Button variant="outline" onclick={() => (limit += PAGE)}>Show more</Button>
	{/if}
</Page>

<Dialog.Root bind:open={rescanOpen}>
	<Dialog.Content>
		<Dialog.Header>
			<Dialog.Title>Rescan the chain</Dialog.Title>
			<Dialog.Description>
				Forgets the transaction history and scans again from this block. Use it if a
				transaction is missing.
			</Dialog.Description>
		</Dialog.Header>
		<Label for="rh">From block</Label>
		<Input id="rh" bind:value={rescanHeight} inputmode="numeric" />
		<Dialog.Footer>
			<Button onclick={rescan}>Rescan</Button>
		</Dialog.Footer>
	</Dialog.Content>
</Dialog.Root>
