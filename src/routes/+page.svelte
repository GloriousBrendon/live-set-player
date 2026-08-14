<script lang="ts">
	import EditorView from '$lib/components/editor/EditorView.svelte';
	import PerformanceView from '$lib/components/performance/PerformanceView.svelte';

	let view = $state<'editor' | 'performance'>('editor');
</script>

<div class="app">
	<button class="view-toggle" onclick={() => (view = view === 'editor' ? 'performance' : 'editor')}>
		{view === 'editor' ? 'Performance view →' : '← Editor'}
	</button>

	{#if view === 'editor'}
		<EditorView />
	{:else}
		<PerformanceView />
	{/if}
</div>

<style>
	.app {
		position: relative;
		height: 100vh;
	}
	.view-toggle {
		position: fixed;
		top: 0.6rem;
		right: 0.6rem;
		z-index: 10;
		background: #1b1f24;
		border: 1px solid #2a2f36;
		color: #9fb3c8;
		border-radius: 0.3rem;
		padding: 0.35rem 0.6rem;
		font-size: 0.75rem;
		cursor: pointer;
		opacity: 0.7;
	}
	.view-toggle:hover {
		opacity: 1;
	}
</style>
