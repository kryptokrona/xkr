<!-- A settings row: title and description on the left, the control on the right. -->
<script lang="ts">
	import type { Snippet } from 'svelte';
	let { title, description = '', children }: { title: string; description?: string; children: Snippet } = $props();
</script>

<div class="flex items-center justify-between gap-6 px-4 py-3">
	<div class="flex min-w-0 flex-col gap-0.5">
		<span class="text-title text-sm">{title}</span>
		{#if description}<span class="text-muted-foreground text-xs">{description}</span>{/if}
	</div>
	<div class="flex shrink-0 items-center gap-2">{@render children()}</div>
</div>
