<script lang="ts">
	// The primary gig screen (docs/SPEC.md §9): optimised for a glance from three
	// metres under stage lighting, not information density. Song title, current
	// section name (very large), bars remaining counting down, next section name,
	// queued-section indicator, count-in indicator, device status banner.

	import { status } from '$lib/stores/status';
	import { project, currentSongIndex } from '$lib/stores/project';
	import DeviceBanner from './DeviceBanner.svelte';
	import type { Section } from '$lib/types';

	const song = $derived($currentSongIndex !== null ? ($project?.songs[$currentSongIndex] ?? null) : null);
	const sections = $derived<Section[]>(song?.sections ?? []);

	const currentSection = $derived(
		$status && $status.section >= 0 ? (sections[$status.section] ?? null) : null
	);

	const queuedLabel = $derived.by(() => {
		const q = $status?.queued;
		if (!q || q === 'none') return null;
		if (q === 'end_of_song') return 'End of song';
		return sections[q.section]?.name ?? `Section ${q.section + 1}`;
	});

	const nextSection = $derived.by(() => {
		if (!$status) return null;
		const q = $status.queued;
		if (q !== 'none' && q !== 'end_of_song') {
			return sections[q.section] ?? null;
		}
		if ($status.section >= 0 && $status.section + 1 < sections.length) {
			return sections[$status.section + 1];
		}
		return null;
	});

	const countingIn = $derived($status?.count_in_beats_remaining != null);
</script>

<div class="performance">
	<DeviceBanner />

	<div class="song-title">{song?.title ?? 'No song armed'}</div>

	{#if countingIn}
		<div class="count-in">
			<div class="count-in-label">Count-in</div>
			<div class="count-in-number">{$status?.count_in_beats_remaining}</div>
		</div>
	{:else if currentSection}
		<div class="section-name">{currentSection.name}</div>

		<div class="bars-remaining">
			<span class="bars-number">{$status?.bars_remaining}</span>
			<span class="bars-label">bars remaining</span>
		</div>

		<div class="next-section">
			{#if nextSection}
				<span class="next-label">Next</span>
				<span class="next-name">{nextSection.name}</span>
			{/if}
			{#if queuedLabel}
				<span class="queued-badge">Queued → {queuedLabel}</span>
			{/if}
		</div>
	{:else}
		<div class="idle">
			{song ? 'Not playing — press Space to start' : 'Arm a song in the editor, then press Space'}
		</div>
	{/if}
</div>

<style>
	.performance {
		display: flex;
		flex-direction: column;
		align-items: center;
		justify-content: center;
		gap: 3vh;
		height: 100vh;
		width: 100%;
		background: #000;
		color: #fff;
		text-align: center;
		font-family:
			system-ui,
			-apple-system,
			'Segoe UI',
			sans-serif;
		box-sizing: border-box;
		overflow: hidden;
	}
	.song-title {
		font-size: 3.2vh;
		font-weight: 600;
		color: #9fb3c8;
		letter-spacing: 0.04em;
		text-transform: uppercase;
	}
	.section-name {
		font-size: 17vh;
		font-weight: 800;
		line-height: 1;
		color: #ffffff;
		max-width: 94vw;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.bars-remaining {
		display: flex;
		align-items: baseline;
		gap: 1.5vw;
	}
	.bars-number {
		font-size: 11vh;
		font-weight: 700;
		color: #ffd60a;
		font-variant-numeric: tabular-nums;
		min-width: 3ch;
	}
	.bars-label {
		font-size: 3vh;
		color: #9fb3c8;
		text-transform: uppercase;
		letter-spacing: 0.04em;
	}
	.next-section {
		display: flex;
		gap: 1.2vw;
		align-items: center;
		font-size: 3.6vh;
		color: #cfd8e3;
		min-height: 6vh;
	}
	.next-label {
		color: #6d7f91;
		text-transform: uppercase;
		font-size: 2.6vh;
		letter-spacing: 0.04em;
	}
	.next-name {
		font-weight: 700;
	}
	.queued-badge {
		background: #ff375f;
		color: #fff;
		padding: 0.3em 0.9em;
		border-radius: 0.5em;
		font-weight: 700;
		font-size: 2.8vh;
	}
	.count-in {
		display: flex;
		flex-direction: column;
		align-items: center;
		gap: 2vh;
	}
	.count-in-label {
		font-size: 4.5vh;
		text-transform: uppercase;
		letter-spacing: 0.12em;
		color: #ffd60a;
		font-weight: 700;
	}
	.count-in-number {
		font-size: 26vh;
		font-weight: 800;
		color: #ffffff;
		font-variant-numeric: tabular-nums;
		line-height: 1;
	}
	.idle {
		font-size: 3.5vh;
		color: #6d7f91;
	}
</style>
