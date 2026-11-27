<script lang="ts">
	import { Switch } from '#lib/components/ui/switch/index.js';
	import { Badge } from '#lib/components/ui/badge/index.js';
	import Page from '#lib/components/Page.svelte';
	import Rows from '#lib/components/Rows.svelte';
	import Field from '#lib/components/Field.svelte';
	import Advanced from '#lib/components/Advanced.svelte';
	import Stat from '#lib/components/Stat.svelte';
	import ApplyBar from '#lib/components/ApplyBar.svelte';
	import NodeAlert from '#lib/components/NodeAlert.svelte';
	import { Draft } from '#lib/draft.svelte.js';
	import { live } from '#lib/live.svelte.js';

	const draft = new Draft();
	const h = $derived(live.status?.hugin ?? null);

	const n = (k: string) => (typeof h?.[k] === 'number' ? (h[k] as number).toLocaleString('en-US') : '–');
	const label = (k: string) => k.replaceAll('_', ' ').replace(/^./, (c) => c.toUpperCase());
	// Flattens the relay's counters (one level of nesting) into rows.
	const rows = $derived.by(() => {
		const out: [string, string][] = [];
		for (const [k, v] of Object.entries(h ?? {})) {
			if (v !== null && typeof v === 'object') {
				for (const [k2, v2] of Object.entries(v as Record<string, unknown>)) out.push([`${label(k)} · ${k2.replaceAll('_', ' ')}`, String(v2)]);
			} else out.push([label(k), v === null ? '–' : typeof v === 'number' ? v.toLocaleString('en-US') : String(v)]);
		}
		return out;
	});
</script>

<Page title="Hugin relay" subtitle="private messaging for Hugin apps">
	{#snippet actions()}
		<Badge variant={h ? 'default' : 'outline'}>{h ? 'on' : 'off'}</Badge>
	{/snippet}

	<NodeAlert />

	{#if draft.value}
		{@const d = draft.value}
		<div class="border-border divide-border divide-y rounded-xl border">
			<Field
				title="Relay Hugin messages"
				description="Hugin apps connect to this node over Hyperswarm. The relay pays its fee to the payout address on P2Pool."
			>
				<Switch bind:checked={d.hugin} disabled={!d.hyper} />
			</Field>
		</div>
		{#if !d.hyper}<p class="text-muted-foreground text-xs">The relay needs Hyperswarm (Settings).</p>{/if}
		<ApplyBar {draft} />
	{/if}

	{#if h}
		<section class="grid grid-cols-2 gap-3 lg:grid-cols-4">
			<Stat label="Apps connected" value={n('clients')} accent />
			<Stat label="Relay nodes" value={n('nodes')} />
			<Stat label="Messages" value={n('messages')} hint="{n('posts_accepted')} posts accepted" />
			<Stat label="Fee shares" value={n('shares_submitted')} hint="{n('shares_accepted')} paid messages" />
		</section>
		<Advanced id="hugin" label="All counters">
			<Rows {rows} />
		</Advanced>
	{/if}
</Page>
