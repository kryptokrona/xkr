<script lang="ts">
	import { Switch } from '#lib/components/ui/switch/index.js';
	import { Input } from '#lib/components/ui/input/index.js';
	import { Label } from '#lib/components/ui/label/index.js';
	import Page from '#lib/components/Page.svelte';
	import Rows from '#lib/components/Rows.svelte';
	import Field from '#lib/components/Field.svelte';
	import Advanced from '#lib/components/Advanced.svelte';
	import ApplyBar from '#lib/components/ApplyBar.svelte';
	import { Draft } from '#lib/draft.svelte.js';
	import { live } from '#lib/live.svelte.js';
	import { theme, THEMES } from '#lib/theme.svelte.js';

	const draft = new Draft();
	const info = $derived(live.info);
	let extra = $state<string | null>(null);
	$effect(() => {
		if (extra === null && draft.value) extra = draft.value.extra_args.join(' ');
	});
	$effect(() => {
		if (draft.value && extra !== null) {
			const args = extra.split(/\s+/).filter(Boolean);
			if (JSON.stringify(args) !== JSON.stringify(draft.value.extra_args)) draft.value.extra_args = args;
		}
	});
</script>

<Page title="Settings">
	<section class="flex flex-col gap-2">
		<h4 class="text-sm">Theme</h4>
		<div class="grid grid-cols-3 gap-3">
			{#each THEMES as t}
				<button
					class="border-border flex items-center gap-3 rounded-xl border p-3 text-left transition-colors
						{theme.current === t.id ? 'ring-primary ring-2' : 'hover:bg-accent/50'}"
					onclick={() => theme.set(t.id)}
				>
					<span class="flex size-9 items-center justify-center rounded-lg border" style="background:{t.bg}">
						<span class="size-3 rounded-full" style="background:{t.swatch}"></span>
					</span>
					<span class="text-sm">{t.label}</span>
				</button>
			{/each}
		</div>
	</section>

	{#if draft.value}
		{@const d = draft.value}
		<section class="flex flex-col gap-2">
			<h4 class="text-sm">Network</h4>
			<div class="border-border divide-border divide-y rounded-xl border">
				<Field title="Hyperswarm" description={info?.hyper_available ? 'Peers and Hugin apps over the Hyperswarm network.' : 'The bridge is not bundled in this build.'}>
					<Switch bind:checked={d.hyper} disabled={!info?.hyper_available} />
				</Field>
			</div>
		</section>

		<Advanced id="settings">
			<section class="flex flex-col gap-2">
				<h4 class="text-sm">Node</h4>
				<div class="border-border divide-border divide-y rounded-xl border">
					<Field title="Fast sync" description="Skip proof-of-work checks up to the last checkpoint.">
						<Switch bind:checked={d.fast_sync} />
					</Field>
					<Field title="Verification threads" description="0: half of the {info?.cores ?? '?'} cores.">
						<Input type="number" min={0} max={info?.cores ?? 64} bind:value={d.verify_threads} class="w-24 font-mono" />
					</Field>
					<Field title="Testnet" description="A separate chain and data folder, for testing. Wallets are kept apart too.">
						<Switch bind:checked={d.testnet} />
					</Field>
				</div>
			</section>

			<section class="flex flex-col gap-2">
				<h4 class="text-sm">Wallet</h4>
				<div class="border-border flex flex-col gap-1.5 rounded-xl border px-4 py-3">
					<Label for="wnode">Node for the wallet</Label>
					<Input id="wnode" bind:value={d.wallet_node} class="font-mono" placeholder="empty: this app's node · or host:port" spellcheck={false} />
					<span class="text-muted-foreground text-xs">
						A remote node lets the wallet work before this node has synchronized.
					</span>
				</div>
			</section>

			<section class="flex flex-col gap-2">
				<h4 class="text-sm">xkrd</h4>
				<div class="border-border flex flex-col gap-1.5 rounded-xl border px-4 py-3">
					<Label for="extra">Extra xkrd arguments</Label>
					{#if extra !== null}<Input id="extra" bind:value={extra} class="font-mono" placeholder="--log-level debug" spellcheck={false} />{/if}
				</div>
				<Rows rows={[['App data', info?.app_data], ['Node data', info?.data_dir], ['Node log', info?.log_file]]} />
			</section>
		</Advanced>
		<ApplyBar {draft} />
	{/if}
</Page>
