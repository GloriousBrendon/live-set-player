<script lang="ts">
	import { onMount } from 'svelte';
	import { startStatusPolling, stopStatusPolling } from '$lib/stores/status';
	import { startDeviceStatusPolling, stopDeviceStatusPolling } from '$lib/stores/device';
	import { installKeyboardShortcuts } from '$lib/keyboard';

	let { children } = $props();

	onMount(() => {
		startStatusPolling();
		startDeviceStatusPolling();
		const removeShortcuts = installKeyboardShortcuts();
		return () => {
			stopStatusPolling();
			stopDeviceStatusPolling();
			removeShortcuts();
		};
	});
</script>

{@render children()}

<style>
	:global(html, body) {
		margin: 0;
		padding: 0;
		background: #0d0f12;
		color: #e6edf3;
		font-family:
			system-ui,
			-apple-system,
			'Segoe UI',
			sans-serif;
	}
	:global(*) {
		box-sizing: border-box;
	}
</style>
