<!-- A page: the 60 px title bar of Aesir (drag region for the window) and a scrolling body. -->
<script lang="ts">
	import type { Snippet } from 'svelte';
	let {
		title,
		subtitle = '',
		actions,
		children
	}: { title: string; subtitle?: string; actions?: Snippet; children: Snippet } = $props();
</script>

<div class="flex h-full min-w-0 flex-1 flex-col">
	<header
		data-tauri-drag-region
		class="border-border flex min-h-[60px] items-center justify-between gap-4 border-b px-8"
	>
		<div data-tauri-drag-region class="flex items-baseline gap-3">
			<h3 data-tauri-drag-region class="text-lg">{title}</h3>
			{#if subtitle}
				<span data-tauri-drag-region class="text-muted-foreground font-mono text-xs">{subtitle}</span>
			{/if}
		</div>
		{#if actions}<div class="flex items-center gap-2">{@render actions()}</div>{/if}
	</header>
	<div class="min-h-0 flex-1 overflow-y-auto">
		<div class="mx-auto flex max-w-5xl flex-col gap-6 px-8 py-6">
			{@render children()}
		</div>
	</div>
</div>
