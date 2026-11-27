<!-- Aesir's login field: one input and an arrow button that lights up when it can go on. -->
<script lang="ts">
	import ArrowRight from '@lucide/svelte/icons/arrow-right';
	import Loader from '@lucide/svelte/icons/loader-2';
	import { fly } from 'svelte/transition';

	let {
		value = $bindable(''),
		placeholder,
		type = 'text',
		ready = false,
		busy = false,
		shake = false,
		onsubmit
	}: {
		value: string;
		placeholder: string;
		type?: string;
		ready?: boolean;
		busy?: boolean;
		shake?: boolean;
		onsubmit: () => void;
	} = $props();
</script>

<form
	class="field"
	class:shake
	in:fly={{ y: 20 }}
	onsubmit={(e) => {
		e.preventDefault();
		if (ready && !busy) onsubmit();
	}}
>
	<!-- svelte-ignore a11y_autofocus -->
	<input {type} {placeholder} bind:value autofocus spellcheck={false} autocomplete="off" />
	<button type="submit" aria-label="Continue" disabled={busy}>
		{#if busy}
			<Loader class="size-5 animate-spin" />
		{:else}
			<ArrowRight class="size-5 transition-colors {ready ? 'text-primary' : 'text-foreground'}" strokeWidth={1.5} />
		{/if}
	</button>
</form>

<style>
	.field {
		display: flex;
		align-items: center;
		gap: 6px;
		padding: 0 6px 0 12px;
		background-color: color-mix(in srgb, var(--card) 70%, transparent);
		border: 1px solid var(--input);
		border-radius: 8px;
		backdrop-filter: blur(7px);
		transition: border-color 100ms ease-in-out;
	}
	.field:focus-within {
		border-color: color-mix(in srgb, var(--foreground) 35%, transparent);
	}
	input {
		width: 220px;
		height: 48px;
		color: var(--foreground);
		background: transparent;
		border: none;
		font-size: 1.1rem;
		user-select: text;
		-webkit-user-select: text;
	}
	input:focus {
		outline: none;
	}
	input::placeholder {
		color: var(--muted-foreground);
	}
	button {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		width: 48px;
		height: 36px;
		border-radius: 5px;
		border: 1px solid var(--border);
		background-color: var(--secondary);
		cursor: pointer;
		transition: background-color 100ms ease-in-out;
	}
	button:hover {
		background-color: var(--accent);
	}
	.shake {
		animation: shake 0.4s;
	}
	@keyframes shake {
		20%,
		60% {
			transform: translateX(-8px);
		}
		40%,
		80% {
			transform: translateX(8px);
		}
	}
</style>
