<script lang="ts">
	import { dndzone } from 'svelte-dnd-action';
	import { project, currentSongIndex } from '$lib/stores/project';
	import * as api from '$lib/api';
	import type { Song } from '$lib/types';

	let dragItems = $state<Song[]>([]);
	$effect(() => {
		dragItems = $project?.songs ?? [];
	});

	function handleConsider(e: CustomEvent<{ items: Song[] }>) {
		dragItems = e.detail.items;
	}

	async function handleFinalize(e: CustomEvent<{ items: Song[] }>) {
		dragItems = e.detail.items;
		const original = $project?.songs;
		if (!original) return;
		const newOrder = dragItems.map((s) => original.findIndex((o) => o.id === s.id));
		const reordered = dragItems;
		project.update((p) => (p ? { ...p, songs: reordered } : p));
		try {
			await api.reorderSongs(newOrder);
		} catch (err) {
			console.error('reorder_songs failed', err);
		}
	}

	async function addSong() {
		const song = await api.addSong();
		project.update((p) => (p ? { ...p, songs: [...p.songs, song] } : p));
		currentSongIndex.set(($project?.songs.length ?? 1) - 1);
	}

	async function removeSong(index: number) {
		await api.removeSong(index);
		project.update((p) => (p ? { ...p, songs: p.songs.filter((_, i) => i !== index) } : p));
		if ($currentSongIndex === index) currentSongIndex.set(null);
		else if ($currentSongIndex !== null && $currentSongIndex > index) {
			currentSongIndex.set($currentSongIndex - 1);
		}
	}

	async function duplicateSong(index: number) {
		const copy = await api.duplicateSong(index);
		project.update((p) => {
			if (!p) return p;
			const songs = [...p.songs];
			songs.splice(index + 1, 0, copy);
			return { ...p, songs };
		});
		if ($currentSongIndex !== null && $currentSongIndex > index) {
			currentSongIndex.set($currentSongIndex + 1);
		}
	}

	function selectSong(index: number) {
		currentSongIndex.set(index);
	}

	let armError = $state<string | null>(null);

	async function armAndSelect(index: number) {
		selectSong(index);
		armError = null;
		try {
			await api.armSong(dragItems[index].id);
		} catch (err) {
			armError = String(err);
			console.error('arm_song failed', err);
		}
	}
</script>

<div class="song-list">
	<div class="header">
		<h2>Songs</h2>
		<button onclick={addSong}>+ Add song</button>
	</div>
	{#if armError}
		<div class="arm-error">Arm failed: {armError}</div>
	{/if}
	<div
		class="items"
		use:dndzone={{ items: dragItems, flipDurationMs: 150 }}
		onconsider={handleConsider}
		onfinalize={handleFinalize}
	>
		{#each dragItems as song, i (song.id)}
			<div class="item" class:selected={$currentSongIndex === i} class:disabled={song.disabled}>
				<button class="song-select" onclick={() => selectSong(i)}>{song.title || 'Untitled'}</button>
				<button class="song-arm" title="Arm for playback" onclick={() => armAndSelect(i)}>▶</button>
				<button class="song-duplicate" title="Duplicate song" onclick={() => duplicateSong(i)}
					>⧉</button
				>
				<button class="song-remove" title="Remove song" onclick={() => removeSong(i)}>✕</button>
			</div>
		{/each}
	</div>
</div>

<style>
	.song-list {
		display: flex;
		flex-direction: column;
		gap: 0.5rem;
		min-width: 16rem;
	}
	.header {
		display: flex;
		justify-content: space-between;
		align-items: center;
	}
	.header h2 {
		margin: 0;
		font-size: 1rem;
		text-transform: uppercase;
		letter-spacing: 0.04em;
		color: #9fb3c8;
	}
	.items {
		display: flex;
		flex-direction: column;
		gap: 0.25rem;
	}
	.item {
		display: flex;
		align-items: center;
		gap: 0.25rem;
		background: #1b1f24;
		border: 1px solid #2a2f36;
		border-radius: 0.4rem;
		padding: 0.25rem;
	}
	.item.selected {
		border-color: #3f8cff;
	}
	.item.disabled {
		opacity: 0.5;
	}
	.song-select {
		flex: 1;
		text-align: left;
		background: none;
		border: none;
		color: inherit;
		padding: 0.4rem;
		cursor: pointer;
		font-size: 0.95rem;
	}
	.song-arm,
	.song-duplicate,
	.song-remove {
		background: none;
		border: none;
		color: #9fb3c8;
		cursor: pointer;
		padding: 0.3rem 0.5rem;
	}
	.song-arm:hover {
		color: #3ddc84;
	}
	.song-duplicate:hover {
		color: #3f8cff;
	}
	.song-remove:hover {
		color: #ff375f;
	}
	.arm-error {
		color: #ff375f;
		font-size: 0.8rem;
	}
</style>
