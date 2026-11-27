<script lang="ts">
	import Pickaxe from '@lucide/svelte/icons/pickaxe';
	import * as Select from '#lib/components/ui/select/index.js';
	import * as Alert from '#lib/components/ui/alert/index.js';
	import { Switch } from '#lib/components/ui/switch/index.js';
	import { Input } from '#lib/components/ui/input/index.js';
	import { Label } from '#lib/components/ui/label/index.js';
	import { Badge } from '#lib/components/ui/badge/index.js';
	import Page from '#lib/components/Page.svelte';
	import Stat from '#lib/components/Stat.svelte';
	import Rows from '#lib/components/Rows.svelte';
	import Field from '#lib/components/Field.svelte';
	import Advanced from '#lib/components/Advanced.svelte';
	import ApplyBar from '#lib/components/ApplyBar.svelte';
	import NodeAlert from '#lib/components/NodeAlert.svelte';
	import PayoutAddress from '#lib/components/PayoutAddress.svelte';
	import { Draft } from '#lib/draft.svelte.js';
	import { live } from '#lib/live.svelte.js';
	import { int, rate, short } from '#lib/format.js';

	const draft = new Draft();
	const m = $derived(live.status?.mining ?? null);
	const info = $derived(live.info);
	const modes = [
		{ value: 'p2pool', label: 'P2Pool (shares, steady payouts)' },
		{ value: 'solo', label: 'Solo (whole blocks to one address)' }
	];
	const state = $derived.by(() => {
		if (!m?.active) return 'off';
		if (!live.status?.synced) return 'waiting for the node to sync';
		if (m.mode === 'p2pool' && live.status?.p2pool?.state !== 'running') return 'waiting for p2pool';
		return `mining with ${m.threads} threads`;
	});
</script>

<Page title="Mining" subtitle="built-in CPU miner · cn-pico/trtl">
	{#snippet actions()}
		<Badge variant={m?.active ? 'default' : 'outline'}>{m?.active ? 'on' : 'off'}</Badge>
	{/snippet}

	<NodeAlert />

	<section class="grid grid-cols-2 gap-3 lg:grid-cols-4">
		<Stat label="Hashrate" value={m?.active ? rate(m.hashrate) : '–'} hint={state} accent={!!m?.active} />
		<Stat label="Total hashes" value={int(m?.total_hashes)} />
		<Stat
			label={m?.mode === 'p2pool' ? 'Shares found' : 'Blocks found'}
			value={int(m?.mode === 'p2pool' ? m?.shares_found : m?.blocks_found)}
			hint={m?.blocks_rejected ? `${m.blocks_rejected} rejected` : ''}
		/>
		<Stat label="Mining block" value={int(m?.height)} hint="difficulty {int(m?.difficulty)}" />
	</section>

	{#if m?.last_error}
		<Alert.Root variant="destructive">
			<Alert.Title>Last miner error</Alert.Title>
			<Alert.Description class="font-mono text-xs">{m.last_error}</Alert.Description>
		</Alert.Root>
	{/if}

	{#if draft.value}
		{@const d = draft.value}
		<section class="flex flex-col gap-2">
			<h4 class="text-sm">Miner</h4>
			<div class="border-border divide-border divide-y rounded-xl border">
				<Field title="Mine with this computer" description="The miner starts hashing once the node is synchronized.">
					<Switch bind:checked={d.mining.enabled} />
				</Field>
				<Field title="Mode">
					<Select.Root type="single" bind:value={d.mining.mode}>
						<Select.Trigger class="w-72">{modes.find((x) => x.value === d.mining.mode)?.label}</Select.Trigger>
						<Select.Content>
							{#each modes as o}<Select.Item value={o.value} label={o.label} />{/each}
						</Select.Content>
					</Select.Root>
				</Field>
				{#if d.mining.mode === 'solo'}
					<div class="flex flex-col gap-1.5 px-4 py-3">
						<Label for="mine-addr">Block reward address</Label>
						<PayoutAddress id="mine-addr" bind:value={d.mining.address} />
					</div>
				{:else}
					<div class="flex flex-col gap-1.5 px-4 py-3">
						<Label for="payout">P2Pool payout address</Label>
						<PayoutAddress id="payout" bind:value={d.payout_address} placeholder="empty: the address the node saved, else the donation address" />
						{#if !d.p2pool.enabled}
							<span class="text-destructive text-xs">P2Pool is turned off; turn it on under P2Pool.</span>
						{/if}
					</div>
				{/if}
			</div>
		</section>
		<Advanced id="mining">
			<div class="border-border divide-border divide-y rounded-xl border">
				<Field title="Threads" description="{info?.cores ?? '?'} cores on this computer; default {info?.default_threads ?? '?'}.">
					<Input type="number" min={1} max={info?.cores ?? 64} bind:value={d.mining.threads} class="w-24 font-mono" />
				</Field>
			</div>
			{#if m?.active}
				<Rows
					rows={[
						['Paid to', short(m.address, 14)],
						['Last block found', short(m.last_block_hash, 10)]
					]}
				/>
			{/if}
			<p class="text-muted-foreground flex items-center gap-2 font-mono text-xs">
				<Pickaxe class="size-3.5" /> Other miners (XMRig, xkr-miner) can mine to this node's P2Pool stratum; see P2Pool.
			</p>
		</Advanced>
		<ApplyBar {draft} />
	{/if}
</Page>
