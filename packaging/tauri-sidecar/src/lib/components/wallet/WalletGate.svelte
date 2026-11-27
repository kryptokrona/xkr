<!-- No wallet open: Aesir's login. A password field for the last wallet, the wallet's name to
     switch files, and Open / Create / Restore. Create and Restore go step by step as in Aesir
     (name, password, then the seed words for a restore). Any file the wallet tools read opens
     here; old C++ files are converted on first open, the original kept as `.backup`. -->
<script lang="ts">
	import { onMount } from 'svelte';
	import { fade } from 'svelte/transition';
	import { open as openDialog } from '@tauri-apps/plugin-dialog';
	import { toast } from 'svelte-sonner';
	import ArrowLeft from '@lucide/svelte/icons/arrow-left';
	import * as Select from '#lib/components/ui/select/index.js';
	import { Button } from '#lib/components/ui/button/index.js';
	import { Textarea } from '#lib/components/ui/textarea/index.js';
	import { Input } from '#lib/components/ui/input/index.js';
	import Field from '../auth/Field.svelte';
	import LoginRibbon from '../auth/LoginRibbon.svelte';
	import StartFlash from '../auth/StartFlash.svelte';
	import { api, type WalletFile } from '#lib/api.js';
	import { live } from '#lib/live.svelte.js';

	type Mode = 'login' | 'create' | 'restore';
	let mode = $state<Mode>('login');
	let step = $state(1);
	let files = $state<WalletFile[]>([]);
	let selected = $state<string>('');
	let loaded = $state(false);
	let busy = $state(false);
	let shake = $state(false);

	let password = $state('');
	let name = $state('');
	let repeat = $state('');
	let seed = $state('');
	let scanHeight = $state('');

	const current = $derived(files.find((f) => f.path === selected));
	const seedCount = $derived(seed.split(/\s+/).filter(Boolean).length);

	onMount(async () => {
		try {
			const r = await api.walletList();
			files = r.wallets;
			selected = files[0]?.path ?? '';
			if (!files.length) mode = 'create';
		} catch (e) {
			toast.error(String(e));
		}
		loaded = true;
	});

	function go(m: Mode) {
		mode = m;
		step = 1;
		password = repeat = seed = '';
	}

	function back() {
		if (step > 1) step--;
		else go('login');
	}

	function wrong() {
		shake = true;
		password = '';
		setTimeout(() => (shake = false), 450);
	}

	async function login() {
		busy = true;
		try {
			await api.walletOpen(selected, password);
			password = '';
			await live.refresh();
		} catch (e) {
			const msg = String(e);
			if (/password/i.test(msg)) wrong();
			else toast.error(`Cannot open the wallet: ${msg}`);
		} finally {
			busy = false;
		}
	}

	async function browse() {
		const path = await openDialog({ multiple: false, directory: false, title: 'Open a wallet file' });
		if (typeof path !== 'string') return;
		if (!files.some((f) => f.path === path)) {
			const stem = (path.split(/[\\/]/).pop() ?? path).replace(/\.[^.]+$/, '');
			files = [{ name: stem, path, modified: 0 }, ...files];
		}
		selected = path;
		go('login');
	}

	function nameStep() {
		const n = name.trim();
		if (files.some((f) => f.name === n && f.path.includes('/wallets'))) {
			toast.error('A wallet with that name exists already');
			return;
		}
		step = 2;
	}

	function repeatStep() {
		if (password !== repeat) {
			toast.error('The passwords are not the same');
			repeat = '';
		} else if (mode === 'create') void create();
		else step = 4;
	}

	async function create() {
		busy = true;
		try {
			// The backup dialog in the wallet layout shows the words once the wallet is open.
			live.backup = await api.walletCreate(name, password);
			password = repeat = '';
			await live.refresh();
		} catch (e) {
			toast.error(`Cannot create the wallet: ${e}`);
		} finally {
			busy = false;
		}
	}

	async function restore() {
		busy = true;
		try {
			await api.walletImportSeed(name, password, seed, Number(scanHeight) || 0);
			password = repeat = seed = '';
			toast.success('Wallet restored, scanning the chain');
			await live.refresh();
		} catch (e) {
			toast.error(`Cannot restore the wallet: ${e}`);
		} finally {
			busy = false;
		}
	}
</script>

<section data-tauri-drag-region class="relative flex flex-1 items-center justify-center overflow-hidden">
	<StartFlash />
	<LoginRibbon />

	{#if loaded}
		{#key `${mode}-${step}`}
			<div class="relative z-[1] flex flex-col items-center gap-8" in:fade={{ duration: 200, delay: 150 }}>
				{#if mode === 'login'}
					<Field type="password" placeholder="Password..." bind:value={password} ready={password.length > 0} {busy} {shake} onsubmit={login} />
				{:else if mode === 'create' || mode === 'restore'}
					<h2 class="text-2xl">{mode === 'create' ? 'Create wallet' : 'Restore wallet'}</h2>
					{#if step === 1}
						<Field placeholder="Wallet name.." bind:value={name} ready={name.trim().length > 0} onsubmit={nameStep} />
						{#if mode === 'create'}
							<span class="text-muted-foreground font-mono text-sm">- or -</span>
							<div class="flex gap-2">
								<Button variant="outline" class="w-28" onclick={browse}>Open</Button>
								<Button variant="outline" class="w-28" onclick={() => go('restore')}>Import</Button>
							</div>
						{/if}
					{:else if step === 2}
						<h4 class="text-muted-foreground -mt-4 text-sm">Secure wallet</h4>
						<Field type="password" placeholder="Password.." bind:value={password} ready={password.length > 0} onsubmit={() => (step = 3)} />
					{:else if step === 3}
						<h4 class="text-muted-foreground -mt-4 text-sm">Repeat password</h4>
						<Field
							type="password"
							placeholder="Password again.."
							bind:value={repeat}
							ready={repeat.length > 0}
							{busy}
							onsubmit={repeatStep}
						/>
					{:else if step === 4}
						<form
							class="flex w-[420px] flex-col gap-3"
							onsubmit={(e) => {
								e.preventDefault();
								if (seedCount === 25) restore();
							}}
						>
							<Textarea bind:value={seed} rows={4} class="bg-card/70 font-mono backdrop-blur" placeholder="Your 25 seed words.." spellcheck={false} />
							<div class="flex items-center gap-3">
								<Input bind:value={scanHeight} inputmode="numeric" class="bg-card/70 w-48 font-mono backdrop-blur" placeholder="Scan from block (0)" />
								<span class="text-muted-foreground flex-1 text-right font-mono text-xs">{seedCount}/25 words</span>
							</div>
							<Button type="submit" disabled={seedCount !== 25 || busy}>Restore</Button>
						</form>
					{/if}
				{/if}

				{#if mode !== 'login' && (step > 1 || files.length > 0)}
					<button class="text-muted-foreground hover:text-foreground transition-colors" onclick={back} aria-label="Back">
						<ArrowLeft class="size-6" strokeWidth={1.5} />
					</button>
				{/if}
			</div>
		{/key}

		{#if mode === 'login'}
			<div class="absolute bottom-8 z-[1] flex flex-col items-center gap-4" in:fade={{ delay: 300 }}>
				{#if files.length > 1}
					<Select.Root type="single" bind:value={selected}>
						<Select.Trigger class="border-none bg-transparent font-mono text-sm shadow-none dark:bg-transparent">
							{current?.name}.wallet
						</Select.Trigger>
						<Select.Content>
							{#each files as f}<Select.Item value={f.path} label={f.name} />{/each}
						</Select.Content>
					</Select.Root>
				{:else}
					<span class="font-mono text-sm">{current?.name}.wallet</span>
				{/if}
				<div class="flex gap-2">
					<Button variant="outline" class="w-32" onclick={browse}>Open</Button>
					<Button variant="outline" class="w-32" onclick={() => go('create')}>Create</Button>
				</div>
			</div>
		{/if}
	{/if}
</section>
