<!-- Label/value rows, Aesir's bordered list look. -->
<script lang="ts">
	let { rows }: { rows: [string, string | number | null | undefined][] } = $props();
</script>

<dl class="border-border divide-border divide-y rounded-xl border">
	{#each rows as [k, v]}
		<div class="flex items-center justify-between gap-6 px-4 py-2.5">
			<dt class="text-muted-foreground text-sm">{k}</dt>
			<dd class="selectable truncate text-right font-mono text-sm">{v ?? '–'}</dd>
		</div>
	{/each}
</dl>
