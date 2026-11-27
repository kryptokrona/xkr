<script lang="ts">
	import { toast } from 'svelte-sonner';
	import RotateCw from '@lucide/svelte/icons/rotate-cw';
	import { Button } from '#lib/components/ui/button/index.js';
	import { Badge } from '#lib/components/ui/badge/index.js';
	import { Progress } from '#lib/components/ui/progress/index.js';
	import Page from '#lib/components/Page.svelte';
	import Stat from '#lib/components/Stat.svelte';
	import Rows from '#lib/components/Rows.svelte';
	import Advanced from '#lib/components/Advanced.svelte';
	import NodeAlert from '#lib/components/NodeAlert.svelte';
	import { api } from '#lib/api.js';
	import { live } from '#lib/live.svelte.js';
	import { int, bytes, duration, percent, short, rate, date } from '#lib/format.js';

	const s = $derived(live.status);
	const info = $derived(live.info);
	const hyper = $derived((s?.hyper ?? null) as Record<string, unknown> | null);
	let restarting = $state(false);

	async function restart() {
		restarting = true;
		try {
			await api.nodeRestart();
			toast.success('Node restarted');
		} catch (e) {
			toast.error(String(e));
		} finally {
			restarting = false;
			await live.refresh();
		}
	}
</script>

<Page title="Node" subtitle={s ? `${s.network} · ${s.version}` : ''}>
	{#snippet actions()}
		<Badge variant={live.phase === 'running' ? 'default' : 'outline'}>{live.phase}</Badge>
		<Button variant="ghost" size="icon" aria-label="Restart node" onclick={restart} disabled={restarting || live.applying}>
			<RotateCw class={restarting ? 'animate-spin' : ''} />
		</Button>
	{/snippet}

	<NodeAlert />

	<section class="flex flex-col gap-2">
		<div class="flex justify-between font-mono text-xs">
			<span class="text-muted-foreground">
				{#if s?.synced}synchronized{:else if s?.download.active}downloading {s.download.blocks_per_second.toFixed(0)} blocks/s · {bytes(s.download.bytes_per_second)}/s{:else}waiting for peers{/if}
			</span>
			<span>{s ? percent(s.sync_progress, 2) : '–'}</span>
		</div>
		<Progress value={(s?.sync_progress ?? 0) * 100} max={100} class="h-1.5" />
	</section>

	<section class="grid grid-cols-2 gap-3 lg:grid-cols-4">
		<Stat label="Height" value={int(s?.height)} hint="network {int(s?.network_height)}" accent />
		<Stat label="Peers" value={int(s?.peers.total)} hint="{s?.peers.legacy_outgoing ?? 0} out · {s?.peers.legacy_incoming ?? 0} in · {s?.peers.hyper ?? 0} swarm" />
		<Stat label="Database" value={bytes(s?.db_size)} hint={s?.alt_blocks ? `${s.alt_blocks} alt blocks` : 'no alt blocks'} />
		<Stat label="Uptime" value={duration(s?.uptime)} hint="since {s ? date(s.start_time) : '–'}" />
	</section>

	<Advanced id="node" label="Details">
		<section class="grid gap-4 lg:grid-cols-2">
			<div class="flex flex-col gap-2">
				<h4 class="text-sm">Chain</h4>
				<Rows
					rows={[
						['Top block', short(s?.top_block_hash, 10)],
						['Top block time', s ? date(s.top_block_timestamp) : null],
						['Block rate', s ? `${s.block_rate.blocks}/${s.block_rate.expected} · ${s.block_rate.status}` : null],
						['Transaction pool', s ? `${s.pool.count} (${bytes(s.pool.bytes)})` : null],
						['Peer lists', s ? `${s.white_peerlist_size} white · ${s.gray_peerlist_size} gray` : null],
						['Network hashrate', live.wallet.sync ? rate(live.wallet.sync.hashrate) : null]
					]}
				/>
			</div>
			<div class="flex flex-col gap-2">
				<h4 class="text-sm">This node</h4>
				<Rows
					rows={[
						['Hyperswarm', !info?.settings.hyper ? 'off' : info?.hyper_available ? (hyper ? `on${hyper.firewalled ? ' · firewalled' : ''}` : 'starting') : 'bridge not bundled'],
						['NAT', hyper?.nat ? String(hyper.nat) : null],
						['RPC', info ? `127.0.0.1:${info.rpc_port}` : null],
						['Fast sync', info?.settings.fast_sync ? 'on' : 'off'],
						['Data', info?.data_dir],
						['Log', info?.log_file]
					]}
				/>
			</div>
		</section>
	</Advanced>
</Page>
