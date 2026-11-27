<script lang="ts">
	import { onDestroy, onMount } from 'svelte';
	import { Switch } from '#lib/components/ui/switch/index.js';
	import { Label } from '#lib/components/ui/label/index.js';
	import Page from '#lib/components/Page.svelte';
	import Copy from '#lib/components/Copy.svelte';
	import { api } from '#lib/api.js';

	interface Line {
		time: string;
		level: string;
		text: string;
	}
	let raw = $state('');
	let follow = $state(true);
	let box = $state<HTMLDivElement | null>(null);
	let timer: ReturnType<typeof setInterval>;

	// xkrd --log-json: one JSON object per line (timestamp, level, fields.message, target);
	// anything else is shown as it is.
	function parse(l: string): Line {
		try {
			const j = JSON.parse(l);
			const f = j.fields ?? {};
			const rest = Object.entries(f)
				.filter(([k]) => k !== 'message')
				.map(([k, v]) => `${k}=${typeof v === 'string' ? v : JSON.stringify(v)}`)
				.join(' ');
			const t = typeof j.timestamp === 'string' ? j.timestamp.slice(11, 19) : '';
			return { time: t, level: String(j.level ?? '').toLowerCase(), text: `${f.message ?? j.message ?? ''} ${rest}`.trim() };
		} catch {
			return { time: '', level: '', text: l };
		}
	}
	const lines = $derived(raw.split('\n').filter(Boolean).map(parse));
	const color: Record<string, string> = { error: 'text-destructive', warn: 'text-warning', debug: 'text-muted-foreground', trace: 'text-muted-foreground' };

	async function load() {
		try {
			raw = await api.nodeLog(400);
		} catch (e) {
			raw = String(e);
		}
		if (follow && box) requestAnimationFrame(() => box && (box.scrollTop = box.scrollHeight));
	}
	onMount(() => {
		void load();
		timer = setInterval(load, 2000);
	});
	onDestroy(() => clearInterval(timer));
</script>

<Page title="Logs" subtitle="xkrd, last 400 lines">
	{#snippet actions()}
		<Label class="text-muted-foreground flex items-center gap-2 text-xs">Follow <Switch bind:checked={follow} /></Label>
		<Copy text={raw} label="Log copied" />
	{/snippet}
	<div bind:this={box} class="bg-card border-border selectable h-[calc(100vh-140px)] overflow-auto rounded-xl border p-3 font-mono text-xs leading-relaxed">
		{#each lines as l}
			<div class="flex gap-3 whitespace-pre-wrap {color[l.level] ?? ''}">
				{#if l.time}<span class="text-muted-foreground shrink-0">{l.time}</span>{/if}
				{#if l.level}<span class="w-10 shrink-0 uppercase">{l.level}</span>{/if}
				<span class="min-w-0 break-all">{l.text}</span>
			</div>
		{/each}
	</div>
</Page>
