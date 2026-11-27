<script lang="ts">
	import CopyIcon from '@lucide/svelte/icons/copy';
	import CheckIcon from '@lucide/svelte/icons/check';
	import { Button } from '#lib/components/ui/button/index.js';
	import { toast } from 'svelte-sonner';
	let { text, label = 'Copied' }: { text: string; label?: string } = $props();
	let done = $state(false);

	async function copy() {
		try {
			await navigator.clipboard.writeText(text);
			done = true;
			toast.success(label);
			setTimeout(() => (done = false), 1500);
		} catch (e) {
			toast.error(`Cannot copy: ${e}`);
		}
	}
</script>

<Button variant="ghost" size="icon-sm" onclick={copy} aria-label="Copy">
	{#if done}<CheckIcon class="text-primary" />{:else}<CopyIcon />{/if}
</Button>
