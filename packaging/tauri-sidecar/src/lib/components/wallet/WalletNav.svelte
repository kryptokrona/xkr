<script lang="ts">
	import { page } from '$app/state';
	const links = [
		{ href: '/wallet', label: 'Overview' },
		{ href: '/wallet/send', label: 'Send' },
		{ href: '/wallet/receive', label: 'Receive' },
		{ href: '/wallet/history', label: 'History' }
	];
</script>

<nav class="bg-muted flex gap-1 rounded-lg p-1">
	{#each links as l}
		<a
			href={l.href}
			class="rounded-md px-3 py-1 text-sm transition-colors
				{page.url.pathname === l.href ? 'bg-background text-title shadow-sm' : 'text-muted-foreground hover:text-foreground'}"
			>{l.label}</a
		>
	{/each}
</nav>
