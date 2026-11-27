<script lang="ts">
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
	import Copy from '#lib/components/Copy.svelte';
	import Advanced from '#lib/components/Advanced.svelte';
	import ApplyBar from '#lib/components/ApplyBar.svelte';
	import NodeAlert from '#lib/components/NodeAlert.svelte';
	import PayoutAddress from '#lib/components/PayoutAddress.svelte';
	import { Draft } from '#lib/draft.svelte.js';
	import { live } from '#lib/live.svelte.js';
	import { int, rate, short } from '#lib/format.js';

	const draft = new Draft();
	const p = $derived(live.status?.p2pool ?? null);
	const chains = [
		{ value: 'default', label: 'Main' },
		{ value: 'mini', label: 'Mini' },
		{ value: 'nano', label: 'Nano' }
	];
	const stateLabel = $derived(
		!p ? 'off' : p.state === 'waiting' ? 'waiting for the node to sync' : p.state
	);
	const xmrig = $derived(
		p?.stratum ? `xmrig -o ${p.stratum.replace('0.0.0.0', '<this computer>')} -a cn-pico/trtl -u x` : ''
	);
</script>

<Page title="P2Pool" subtitle={p ? `${p.sidechain} sidechain` : ''}>
	{#snippet actions()}
		<Badge variant={p?.state === 'running' ? 'default' : 'outline'}>{stateLabel}</Badge>
	{/snippet}

	<NodeAlert />
	{#if p?.error}
		<Alert.Root variant="destructive">
			<Alert.Title>P2Pool failed</Alert.Title>
			<Alert.Description class="font-mono text-xs">{p.error}</Alert.Description>
		</Alert.Root>
	{/if}

	<section class="grid grid-cols-2 gap-3 lg:grid-cols-4">
		<Stat label="Sidechain hashrate" value={p ? rate(p.hashrate) : '–'} hint="{int(p?.miners)} miners in 72 h" accent={p?.state === 'running'} />
		<Stat label="Sidechain height" value={int(p?.height)} hint="difficulty {int(p?.difficulty)}" />
		<Stat label="Found here" value="{int(p?.shares_found)} shares" hint="{int(p?.blocks_found)} blocks" />
		<Stat label="Stratum" value="{int(p?.stratum_miners)} miners" hint={p ? rate(p.stratum_hashrate) : ''} />
	</section>

	{#if draft.value}
		{@const d = draft.value}
		<section class="flex flex-col gap-2">
			<h4 class="text-sm">Settings</h4>
			<div class="border-border divide-border divide-y rounded-xl border">
				<Field title="Run P2Pool" description="A decentralized pool in the node: miners share the block rewards.">
					<Switch bind:checked={d.p2pool.enabled} />
				</Field>
				<div class="flex flex-col gap-1.5 px-4 py-3">
					<Label for="payout">Payout address</Label>
					<PayoutAddress id="payout" bind:value={d.payout_address} placeholder="empty: the address the node saved, else the donation address" />
					<span class="text-muted-foreground text-xs">Also the Hugin relay's fee address.</span>
				</div>
			</div>
		</section>
		<Advanced id="p2pool">
			<div class="border-border divide-border divide-y rounded-xl border">
				<Field title="Sidechain" description="Mini and nano have lower share difficulty, for small miners.">
					<Select.Root type="single" bind:value={d.p2pool.sidechain}>
						<Select.Trigger class="w-32">{chains.find((c) => c.value === d.p2pool.sidechain)?.label}</Select.Trigger>
						<Select.Content>
							{#each chains as c}<Select.Item value={c.value} label={c.label} />{/each}
						</Select.Content>
					</Select.Root>
				</Field>
				<Field
					title="Accept miners from the network"
					description="Off: only miners on this computer. On: anyone who reaches the port mines for your payout address (no login)."
				>
					<Switch bind:checked={d.p2pool.stratum_lan} />
				</Field>
				<Field title="Stratum port">
					<Input type="number" min={1} max={65535} bind:value={d.p2pool.stratum_port} class="w-28 font-mono" />
				</Field>
			</div>
			{#if p}
				<Rows
					rows={[
						['Synced', p.synced ? 'yes' : 'no'],
						['Shares stored', int(p.shares)],
						['Peers', `${p.peers_out} out · ${p.peers_in} in · ${p.peers_hyper} swarm`],
						['Payout', p.payout_address ? `${short(p.payout_address, 12)}${p.payout_donation ? ' (donation)' : ''}` : 'none'],
						['Stratum', p.stratum ?? p.stratum_off ?? 'off'],
						['Consensus id', short(p.consensus_id, 10)]
					]}
				/>
				{#if xmrig}
					<div class="bg-card border-border flex items-center gap-2 rounded-xl border px-4 py-2">
						<code class="selectable text-muted-foreground min-w-0 flex-1 truncate font-mono text-xs">{xmrig}</code>
						<Copy text={xmrig} label="Command copied" />
					</div>
				{/if}
			{/if}
		</Advanced>
		<ApplyBar {draft} />
	{/if}
</Page>
