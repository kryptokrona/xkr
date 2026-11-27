<script lang="ts">
	import { goto } from '$app/navigation';
	import { toast } from 'svelte-sonner';
	import LogOut from '@lucide/svelte/icons/log-out';
	import KeyRound from '@lucide/svelte/icons/key-round';
	import Send from '@lucide/svelte/icons/send';
	import Download from '@lucide/svelte/icons/download';
	import * as Dialog from '#lib/components/ui/dialog/index.js';
	import { Button } from '#lib/components/ui/button/index.js';
	import { Input } from '#lib/components/ui/input/index.js';
	import { Label } from '#lib/components/ui/label/index.js';
	import { Badge } from '#lib/components/ui/badge/index.js';
	import Page from '#lib/components/Page.svelte';
	import Copy from '#lib/components/Copy.svelte';
	import WalletNav from '#lib/components/wallet/WalletNav.svelte';
	import TxRow from '#lib/components/wallet/TxRow.svelte';
	import SyncBar from '#lib/components/wallet/SyncBar.svelte';
	import SeedWords from '#lib/components/wallet/SeedWords.svelte';
	import { api, type WalletTx } from '#lib/api.js';
	import { live } from '#lib/live.svelte.js';
	import { xkr } from '#lib/format.js';

	const w = $derived(live.wallet);
	let txs = $state<WalletTx[]>([]);
	let seedOpen = $state(false);
	let seedPassword = $state('');
	let seed = $state<string | null>(null);

	$effect(() => {
		// Reload when the wallet height moves.
		void w.sync?.wallet_height;
		api.walletTransactions(8).then((r) => (txs = r.transactions)).catch(() => {});
	});

	async function showSeed() {
		try {
			seed = await api.walletMnemonic(seedPassword);
		} catch (e) {
			toast.error(String(e));
		}
		seedPassword = '';
	}

	async function close() {
		try {
			await api.walletClose();
			await live.refresh();
		} catch (e) {
			toast.error(`Cannot close the wallet: ${e}`);
		}
	}
</script>

<Page title={w.name ?? 'Wallet'} subtitle={w.view_only ? 'view only' : ''}>
	{#snippet actions()}
		<WalletNav />
		<Button variant="ghost" size="icon" aria-label="Seed words" onclick={() => (seedOpen = true)}><KeyRound /></Button>
		<Button variant="ghost" size="icon" aria-label="Close wallet" onclick={close}><LogOut /></Button>
	{/snippet}

	<section class="flex flex-col gap-4">
		<div class="flex items-end justify-between gap-6">
			<div class="flex flex-col gap-1">
				<span class="text-muted-foreground text-xs tracking-wide uppercase">Balance</span>
				<div class="flex items-baseline gap-2">
					<span class="text-primary selectable font-mono text-4xl">{xkr(w.balance?.unlocked)}</span>
					<span class="text-muted-foreground text-lg">XKR</span>
				</div>
				{#if w.balance?.locked}
					<span class="text-warning font-mono text-xs">+ {xkr(w.balance.locked)} XKR locked</span>
				{/if}
			</div>
			<div class="flex gap-2">
				<Button onclick={() => goto('/wallet/send')}><Send /> Send</Button>
				<Button variant="outline" onclick={() => goto('/wallet/receive')}><Download /> Receive</Button>
			</div>
		</div>
		<SyncBar />
	</section>

	<section class="bg-card border-border flex items-center gap-3 rounded-xl border px-4 py-3">
		<span class="text-muted-foreground text-xs uppercase">Address</span>
		<span class="selectable min-w-0 flex-1 truncate font-mono text-sm">{w.address}</span>
		<Copy text={w.address ?? ''} label="Address copied" />
		{#if w.format && w.format !== 'WalletBackend'}<Badge variant="outline">{w.format}</Badge>{/if}
	</section>

	<section class="flex flex-col gap-2">
		<div class="flex items-center justify-between">
			<h4 class="text-sm">Recent transactions</h4>
			<a href="/wallet/history" class="text-muted-foreground hover:text-foreground text-xs">All transactions →</a>
		</div>
		<div class="border-border overflow-hidden rounded-xl border">
			{#each txs as tx (tx.hash)}
				<TxRow {tx} />
			{:else}
				<p class="text-muted-foreground px-4 py-10 text-center text-sm">No transactions yet.</p>
			{/each}
		</div>
	</section>
</Page>

<Dialog.Root
	bind:open={seedOpen}
	onOpenChange={(o) => {
		if (!o) seed = null;
	}}
>
	<Dialog.Content class="sm:max-w-2xl">
		<Dialog.Header>
			<Dialog.Title>Seed words</Dialog.Title>
			<Dialog.Description>Anyone with these words can spend this wallet's XKR.</Dialog.Description>
		</Dialog.Header>
		{#if seed}
			<SeedWords mnemonic={seed} />
		{:else}
			<form
				class="flex flex-col gap-3"
				onsubmit={(e) => {
					e.preventDefault();
					showSeed();
				}}
			>
				<Label for="seed-pw">Wallet password</Label>
				<Input id="seed-pw" type="password" bind:value={seedPassword} />
				<Button type="submit" disabled={!seedPassword}>Show seed words</Button>
			</form>
		{/if}
	</Dialog.Content>
</Dialog.Root>
