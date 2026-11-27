<!-- The node is not running: why, with the last log lines. -->
<script lang="ts">
	import * as Alert from '#lib/components/ui/alert/index.js';
	import { live } from '#lib/live.svelte.js';
	const show = $derived(!!live.info && ['failed', 'exited'].includes(live.info.phase));
</script>

{#if show}
	<Alert.Root variant="destructive">
		<Alert.Title>The node {live.info?.phase === 'failed' ? 'did not start' : 'stopped'}</Alert.Title>
		<Alert.Description>
			<pre class="selectable max-h-40 overflow-auto font-mono text-xs whitespace-pre-wrap">{live.info?.detail}</pre>
		</Alert.Description>
	</Alert.Root>
{:else if live.phase === 'starting' || live.phase === 'restarting' || live.phase === 'stopping'}
	<Alert.Root>
		<Alert.Title class="blink">Node {live.phase}…</Alert.Title>
		<Alert.Description>Opening the database and starting the RPC.</Alert.Description>
	</Alert.Root>
{/if}
