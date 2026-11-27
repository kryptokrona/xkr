<script lang="ts">
	import { goto } from '$app/navigation';
	import { onMount } from 'svelte';
	onMount(() => goto('/wallet', { replaceState: true }));
</script>
