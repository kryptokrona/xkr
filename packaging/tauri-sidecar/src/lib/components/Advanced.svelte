<!-- Settings and details most people never touch, folded away; open state kept per page. -->
<script lang="ts">
	import type { Snippet } from 'svelte';
	import { slide } from 'svelte/transition';
	import ChevronRight from '@lucide/svelte/icons/chevron-right';
	let { id, label = 'Advanced', children }: { id: string; label?: string; children: Snippet } = $props();

	const key = () => `xkr-advanced-${id}`;
	let open = $state(false);
	try {
		open = localStorage.getItem(key()) === '1';
	} catch {
		// closed by default
	}
	function toggle() {
		open = !open;
		try {
			localStorage.setItem(key(), open ? '1' : '0');
		} catch {
			// not kept
		}
	}
</script>

<section class="flex flex-col gap-3">
	<button
		class="text-muted-foreground hover:text-foreground flex w-fit items-center gap-1 text-sm transition-colors"
		onclick={toggle}
		aria-expanded={open}
	>
		<ChevronRight class="size-4 transition-transform {open ? 'rotate-90' : ''}" />
		{label}
	</button>
	{#if open}
		<div class="flex flex-col gap-4" transition:slide={{ duration: 150 }}>{@render children()}</div>
	{/if}
</section>
