<script lang="ts">
	import Page from '#lib/components/Page.svelte';
	import Copy from '#lib/components/Copy.svelte';
	import Logo from '#lib/components/Logo.svelte';
	import WalletNav from '#lib/components/wallet/WalletNav.svelte';
	import { live } from '#lib/live.svelte.js';
	const w = $derived(live.wallet);
</script>

<Page title="Receive">
	{#snippet actions()}<WalletNav />{/snippet}

	<div class="mx-auto flex w-full max-w-xl flex-col items-center gap-6 pt-8">
		<div class="text-primary"><Logo size={64} /></div>
		<p class="text-muted-foreground text-center text-sm">Share this address to receive XKR.</p>
		{#each w.addresses ?? [] as a, i}
			<div class="bg-card border-border flex w-full items-start gap-2 rounded-xl border p-4">
				<div class="flex min-w-0 flex-1 flex-col gap-1">
					<span class="text-muted-foreground text-xs uppercase">{i === 0 ? 'Primary address' : `Address ${i + 1}`}</span>
					<span class="selectable font-mono text-sm break-all">{a}</span>
				</div>
				<Copy text={a} label="Address copied" />
			</div>
		{/each}
	</div>
</Page>
