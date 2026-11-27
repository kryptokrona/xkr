<!-- The 80 px icon rail of Aesir: sections on top, settings and node state at the bottom. -->
<script lang="ts">
	import { page } from '$app/state';
	import * as Tooltip from '#lib/components/ui/tooltip/index.js';
	import WalletIcon from '@lucide/svelte/icons/wallet';
	import ServerIcon from '@lucide/svelte/icons/server';
	import PickaxeIcon from '@lucide/svelte/icons/pickaxe';
	import WavesIcon from '@lucide/svelte/icons/waves';
	import MessagesIcon from '@lucide/svelte/icons/messages-square';
	import ScrollIcon from '@lucide/svelte/icons/scroll-text';
	import SettingsIcon from '@lucide/svelte/icons/settings';
	import Logo from './Logo.svelte';
	import Dot from './Dot.svelte';
	import { live } from '#lib/live.svelte.js';

	const top = [
		{ href: '/wallet', label: 'Wallet', icon: WalletIcon },
		{ href: '/node', label: 'Node', icon: ServerIcon },
		{ href: '/mining', label: 'Mining', icon: PickaxeIcon },
		{ href: '/p2pool', label: 'P2Pool', icon: WavesIcon },
		{ href: '/hugin', label: 'Hugin relay', icon: MessagesIcon }
	];
	const bottom = [
		{ href: '/logs', label: 'Logs', icon: ScrollIcon },
		{ href: '/settings', label: 'Settings', icon: SettingsIcon }
	];

	const active = (href: string) => page.url.pathname === href || page.url.pathname.startsWith(href + '/');
	const nodeState = $derived.by(() => {
		const p = live.phase;
		if (p === 'running') return live.status?.synced ? 'ok' : 'warn';
		if (p === 'failed' || p === 'exited') return 'bad';
		return 'warn';
	});
	const nodeLabel = $derived.by(() => {
		const s = live.status;
		if (live.phase !== 'running') return `Node ${live.phase}`;
		if (!s) return 'Node running';
		return s.synced ? `Synced · ${s.height.toLocaleString('en-US')}` : `Syncing ${(s.sync_progress * 100).toFixed(1)} %`;
	});
</script>

{#snippet item(link: (typeof top)[number])}
	<Tooltip.Root>
		<Tooltip.Trigger>
			{#snippet child({ props })}
				<a
					{...props}
					href={link.href}
					class="flex size-12 items-center justify-center rounded-lg transition-colors duration-150
						{active(link.href) ? 'bg-accent text-primary' : 'text-foreground hover:bg-accent'}"
					aria-label={link.label}
				>
					<link.icon class="size-[22px]" strokeWidth={1.6} />
				</a>
			{/snippet}
		</Tooltip.Trigger>
		<Tooltip.Content side="right">{link.label}</Tooltip.Content>
	</Tooltip.Root>
{/snippet}

<nav
	data-tauri-drag-region
	class="border-border flex h-full w-20 shrink-0 flex-col items-center justify-between border-r pt-10 pb-4"
>
	<div class="flex flex-col items-center gap-2">
		<a href="/wallet" class="text-primary mb-4" aria-label="xkr"><Logo size={30} /></a>
		{#each top as link}{@render item(link)}{/each}
	</div>
	<div data-tauri-drag-region class="w-full flex-1"></div>
	<div class="flex flex-col items-center gap-2">
		{#each bottom as link}{@render item(link)}{/each}
		<Tooltip.Root>
			<Tooltip.Trigger>
				{#snippet child({ props })}
					<a {...props} href="/node" class="flex size-12 items-center justify-center" aria-label={nodeLabel}>
						<Dot state={nodeState} pulse={nodeState === 'warn'} />
					</a>
				{/snippet}
			</Tooltip.Trigger>
			<Tooltip.Content side="right">{nodeLabel}</Tooltip.Content>
		</Tooltip.Root>
	</div>
</nav>
