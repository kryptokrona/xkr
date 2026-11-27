<!-- A status light: ok (primary), warn, bad, off. -->
<script lang="ts">
	let { state = 'off', pulse = false }: { state?: 'ok' | 'warn' | 'bad' | 'off'; pulse?: boolean } =
		$props();
	const color = $derived(
		{ ok: 'bg-primary', warn: 'bg-warning', bad: 'bg-destructive', off: 'bg-muted-foreground/40' }[state]
	);
</script>

<span class="inline-block size-2 shrink-0 rounded-full {color} {pulse ? 'blink' : ''}"></span>
