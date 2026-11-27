<!-- Shown while a page's settings differ from the running ones. -->
<script lang="ts">
	import Loader from '@lucide/svelte/icons/loader-2';
	import { Button } from '#lib/components/ui/button/index.js';
	import type { Draft } from '#lib/draft.svelte.js';
	import { live } from '#lib/live.svelte.js';
	let { draft, label = 'Apply' }: { draft: Draft; label?: string } = $props();
</script>

{#if draft.dirty || live.applying}
	<div class="bg-popover border-border sticky bottom-4 flex items-center justify-between gap-4 rounded-xl border px-4 py-3 shadow-lg">
		<span class="text-muted-foreground text-sm">Applying restarts the node; it takes a few seconds.</span>
		<div class="flex gap-2">
			<Button variant="ghost" onclick={() => draft.reset()} disabled={live.applying}>Discard</Button>
			<Button onclick={() => draft.apply()} disabled={live.applying}>
				{#if live.applying}<Loader class="animate-spin" />{/if}
				{label}
			</Button>
		</div>
	</div>
{/if}
