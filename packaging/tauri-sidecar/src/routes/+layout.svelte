<script lang="ts">
	import './layout.css';
	import { onMount } from 'svelte';
	import type { LayoutProps } from './$types';
	import * as Tooltip from '#lib/components/ui/tooltip/index.js';
	import { Toaster } from '#lib/components/ui/sonner/index.js';
	import Rail from '#lib/components/Rail.svelte';
	import { live } from '#lib/live.svelte.js';
	import { theme } from '#lib/theme.svelte.js';

	let { children }: LayoutProps = $props();

	onMount(() => {
		theme.init();
		live.start();
	});
</script>

<svelte:head><title>xkr</title></svelte:head>

<Tooltip.Provider delayDuration={200}>
	<div class="bg-background text-foreground flex h-screen w-screen overflow-hidden">
		<Rail />
		<main class="flex min-w-0 flex-1">{@render children()}</main>
	</div>
</Tooltip.Provider>
<Toaster theme={theme.dark ? 'dark' : 'light'} position="bottom-right" richColors />
