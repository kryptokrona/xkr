<!-- An address input with "use my wallet's address". -->
<script lang="ts">
	import WalletIcon from '@lucide/svelte/icons/wallet';
	import { Input } from '#lib/components/ui/input/index.js';
	import { Button } from '#lib/components/ui/button/index.js';
	import * as Tooltip from '#lib/components/ui/tooltip/index.js';
	import { live } from '#lib/live.svelte.js';
	let { value = $bindable(''), id, placeholder = 'SEKR…' }: { value: string; id: string; placeholder?: string } = $props();
</script>

<div class="flex gap-2">
	<Input {id} bind:value class="font-mono" {placeholder} spellcheck={false} autocomplete="off" />
	<Tooltip.Root>
		<Tooltip.Trigger>
			{#snippet child({ props })}
				<Button
					{...props}
					variant="outline"
					size="icon"
					aria-label="Use my wallet's address"
					disabled={!live.wallet.address}
					onclick={() => (value = live.wallet.address ?? '')}><WalletIcon /></Button
				>
			{/snippet}
		</Tooltip.Trigger>
		<Tooltip.Content>{live.wallet.address ? "Use the open wallet's address" : 'Open a wallet to use its address'}</Tooltip.Content>
	</Tooltip.Root>
</div>
