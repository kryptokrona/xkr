<script lang="ts">
	import { toast } from 'svelte-sonner';
	import Loader from '@lucide/svelte/icons/loader-2';
	import * as Dialog from '#lib/components/ui/dialog/index.js';
	import * as Alert from '#lib/components/ui/alert/index.js';
	import { Button } from '#lib/components/ui/button/index.js';
	import { Input } from '#lib/components/ui/input/index.js';
	import { Label } from '#lib/components/ui/label/index.js';
	import Page from '#lib/components/Page.svelte';
	import Rows from '#lib/components/Rows.svelte';
	import WalletNav from '#lib/components/wallet/WalletNav.svelte';
	import { api } from '#lib/api.js';
	import { live } from '#lib/live.svelte.js';
	import { xkr, parseXkr, short } from '#lib/format.js';

	let address = $state('');
	let amountText = $state('');
	let paymentId = $state('');
	let addressError = $state<string | null>(null);
	let confirming = $state(false);
	let sending = $state(false);
	let sentHash = $state<string | null>(null);

	const w = $derived(live.wallet);
	const amount = $derived(parseXkr(amountText));
	const nodeFee = $derived(w.node?.fee_amount ?? 0);
	const unlocked = $derived(w.balance?.unlocked ?? 0);
	const tooMuch = $derived(amount !== null && amount + nodeFee > unlocked);
	const integrated = $derived(address.trim().length > 99);
	const ready = $derived(
		!!address.trim() && !addressError && amount !== null && amount > 0 && !tooMuch && !w.view_only
	);

	let checkTimer: ReturnType<typeof setTimeout> | undefined;
	$effect(() => {
		const a = address.trim();
		clearTimeout(checkTimer);
		addressError = null;
		if (!a) return;
		checkTimer = setTimeout(() => {
			api.checkAddress(a).then(
				() => (addressError = null),
				(e) => (addressError = String(e))
			);
		}, 300);
	});

	function max() {
		const m = Math.max(0, unlocked - nodeFee - 1000);
		amountText = (m / 100000).toFixed(5);
	}

	async function send() {
		if (amount === null) return;
		sending = true;
		try {
			sentHash = await api.walletSend(address.trim(), amount, integrated ? '' : paymentId.trim());
			toast.success('Transaction sent');
			address = amountText = paymentId = '';
			await live.refresh();
		} catch (e) {
			toast.error(`Not sent: ${e}`);
		} finally {
			sending = false;
			confirming = false;
		}
	}
</script>

<Page title="Send">
	{#snippet actions()}<WalletNav />{/snippet}

	<div class="mx-auto flex w-full max-w-xl flex-col gap-5">
		{#if w.view_only}
			<Alert.Root variant="destructive">
				<Alert.Title>View-only wallet</Alert.Title>
				<Alert.Description>This wallet has no spend key and cannot send.</Alert.Description>
			</Alert.Root>
		{/if}
		<form
			class="flex flex-col gap-4"
			onsubmit={(e) => {
				e.preventDefault();
				if (ready) confirming = true;
			}}
		>
			<div class="flex flex-col gap-1.5">
				<Label for="to">To address</Label>
				<Input id="to" bind:value={address} class="font-mono" placeholder="SEKR…" spellcheck={false} aria-invalid={!!addressError} />
				{#if addressError}<span class="text-destructive text-xs">{addressError}</span>{/if}
			</div>
			<div class="flex flex-col gap-1.5">
				<div class="flex items-center justify-between">
					<Label for="amount">Amount (XKR)</Label>
					<button type="button" class="text-muted-foreground hover:text-primary font-mono text-xs" onclick={max}>
						available {xkr(unlocked)}
					</button>
				</div>
				<Input id="amount" bind:value={amountText} class="font-mono" inputmode="decimal" placeholder="0.00000" aria-invalid={tooMuch || (amountText !== '' && amount === null)} />
				{#if tooMuch}<span class="text-destructive text-xs">More than the unlocked balance (with the node fee).</span>{/if}
			</div>
			<div class="flex flex-col gap-1.5">
				<Label for="pid">Payment ID (optional)</Label>
				<Input id="pid" bind:value={paymentId} class="font-mono" disabled={integrated} placeholder={integrated ? 'in the integrated address' : '64 hex characters'} spellcheck={false} />
			</div>
			{#if nodeFee}
				<p class="text-muted-foreground font-mono text-xs">
					The node charges {xkr(nodeFee)} XKR per transaction, added on top. The network fee is added
					by the wallet.
				</p>
			{/if}
			<Button type="submit" disabled={!ready}>Review</Button>
		</form>

		{#if sentHash}
			<Alert.Root>
				<Alert.Title>Sent</Alert.Title>
				<Alert.Description class="selectable font-mono break-all">{sentHash}</Alert.Description>
			</Alert.Root>
		{/if}
	</div>
</Page>

<Dialog.Root bind:open={confirming}>
	<Dialog.Content>
		<Dialog.Header>
			<Dialog.Title>Send {xkr(amount)} XKR?</Dialog.Title>
			<Dialog.Description>Transactions cannot be undone once they are in a block.</Dialog.Description>
		</Dialog.Header>
		<Rows
			rows={[
				['To', short(address.trim(), 14)],
				['Amount', `${xkr(amount)} XKR`],
				['Node fee', `${xkr(nodeFee)} XKR`],
				['Payment ID', integrated ? 'in the address' : paymentId.trim() || 'none']
			]}
		/>
		<Dialog.Footer>
			<Button variant="outline" onclick={() => (confirming = false)} disabled={sending}>Cancel</Button>
			<Button onclick={send} disabled={sending}>
				{#if sending}<Loader class="animate-spin" />{/if} Send
			</Button>
		</Dialog.Footer>
	</Dialog.Content>
</Dialog.Root>
