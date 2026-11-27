<!-- One figure in a card: label, value (monospace), optional hint. -->
<script lang="ts">
	import type { Snippet } from 'svelte';
	let {
		label,
		value,
		hint = '',
		accent = false,
		children
	}: { label: string; value?: string; hint?: string; accent?: boolean; children?: Snippet } =
		$props();
</script>

<div class="bg-card border-border flex min-w-0 flex-col gap-1 rounded-xl border px-4 py-3">
	<span class="text-muted-foreground text-xs tracking-wide uppercase">{label}</span>
	{#if children}
		{@render children()}
	{:else}
		<span class="truncate font-mono text-lg {accent ? 'text-primary' : 'text-title'}">{value}</span>
	{/if}
	{#if hint}<span class="text-muted-foreground truncate font-mono text-xs">{hint}</span>{/if}
</div>
