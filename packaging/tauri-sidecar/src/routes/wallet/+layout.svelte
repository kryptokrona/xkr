<script lang="ts">
	import type { LayoutProps } from './$types';
	import * as Dialog from '#lib/components/ui/dialog/index.js';
	import { Button } from '#lib/components/ui/button/index.js';
	import { Skeleton } from '#lib/components/ui/skeleton/index.js';
	import WalletGate from '#lib/components/wallet/WalletGate.svelte';
	import SeedWords from '#lib/components/wallet/SeedWords.svelte';
	import { live } from '#lib/live.svelte.js';

	let { children }: LayoutProps = $props();
</script>

{#if !live.loaded}
	<div class="flex flex-1 flex-col gap-4 p-8">
		<Skeleton class="h-10 w-48" />
		<Skeleton class="h-32 w-full" />
	</div>
{:else if live.wallet.open}
	{@render children()}
{:else}
	<WalletGate />
{/if}

<Dialog.Root open={live.backup !== null} onOpenChange={(o) => !o && (live.backup = null)}>
	<Dialog.Content class="sm:max-w-2xl">
		<Dialog.Header>
			<Dialog.Title>Write down your seed words</Dialog.Title>
			<Dialog.Description>
				These 25 words restore the wallet on any device. Anyone who has them can spend your XKR.
				Keep them offline; you can show them again later with your password.
			</Dialog.Description>
		</Dialog.Header>
		{#if live.backup}<SeedWords mnemonic={live.backup.mnemonic} />{/if}
		<Dialog.Footer>
			<Button onclick={() => (live.backup = null)}>I have written them down</Button>
		</Dialog.Footer>
	</Dialog.Content>
</Dialog.Root>
