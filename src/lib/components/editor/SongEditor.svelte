<script lang="ts">
	import { dndzone } from 'svelte-dnd-action';
	import { open as openDialog } from '@tauri-apps/plugin-dialog';
	import { project, currentSongIndex } from '$lib/stores/project';
	import * as api from '$lib/api';
	import { debounce } from '$lib/utils';
	import type { Section, Song, Track } from '$lib/types';

	let songIndex = $derived($currentSongIndex);
	let song = $derived(songIndex !== null ? ($project?.songs[songIndex] ?? null) : null);
	let sampleRate = $derived($project?.sample_rate ?? 48000);

	// Local editable draft, resynced whenever the underlying song identity changes
	// (a different song selected, or a server-confirmed save swapped it in).
	let draft = $state<Song | null>(null);
	let lastSyncedId: string | null = null;
	let saveError = $state<string | null>(null);
	let cueWarnings = $state<string[]>([]);

	// --- Sections -----------------------------------------------------------
	//
	// `sectionRows` wraps each `Section` with a stable `dndId` for the drag list's
	// keyed `{#each}`. That id must NOT change on a plain field edit -- draft.sections
	// is `$state`-proxied, so mutating `row.section.name` in place already updates
	// `draft.sections[i].name` (same underlying object) without any array
	// reassignment. Rebuilding this array (and thus minting new ids) belongs only to
	// structural changes: switching songs, or add/remove/reorder -- never to typing.
	// Getting this wrong once already cost a focus-loss bug: every keystroke
	// regenerated every row's id, so Svelte tore down and recreated every `<input>`
	// on every character.
	let sectionSeq = 0;
	type SectionRow = { dndId: string; section: Section };
	let sectionRows = $state<SectionRow[]>([]);

	$effect(() => {
		if (song && song.id !== lastSyncedId) {
			draft = structuredClone(song);
			lastSyncedId = song.id;
			saveError = null;
			cueWarnings = [];
			sectionRows = draft.sections.map((section) => ({ dndId: `s${sectionSeq++}`, section }));
		} else if (!song) {
			draft = null;
			lastSyncedId = null;
			sectionRows = [];
		}
	});

	const commit = debounce(async () => {
		if (!draft || songIndex === null) return;
		const index = songIndex;
		const payload = draft;
		try {
			const result = await api.updateSong(index, payload);
			project.update((p) => {
				if (!p) return p;
				const songs = [...p.songs];
				songs[index] = result.song;
				return { ...p, songs };
			});
			lastSyncedId = result.song.id;
			cueWarnings = result.cue_warnings;
			saveError = null;
		} catch (err) {
			saveError = String(err);
		}
	}, 600);

	// Field edits mutate `draft` in place (deep `$state` reactivity propagates that
	// on its own) and just need a debounced save -- no reassignment here, see the
	// comment above `sectionRows`.
	function touch() {
		commit();
	}

	function addSection() {
		if (!draft) return;
		const section: Section = {
			name: 'New Section',
			start_bar: 1,
			length_bars: 8,
			loopable: false,
			cue_text: null,
			cue_lead_beats: 4
		};
		draft.sections = [...draft.sections, section];
		sectionRows = [...sectionRows, { dndId: `s${sectionSeq++}`, section }];
		touch();
	}

	function removeSection(i: number) {
		if (!draft) return;
		draft.sections = draft.sections.filter((_, idx) => idx !== i);
		sectionRows = sectionRows.filter((_, idx) => idx !== i);
		touch();
	}

	function handleSectionConsider(e: CustomEvent<{ items: SectionRow[] }>) {
		sectionRows = e.detail.items;
	}
	function handleSectionFinalize(e: CustomEvent<{ items: SectionRow[] }>) {
		sectionRows = e.detail.items;
		if (!draft) return;
		draft.sections = sectionRows.map((r) => r.section);
		touch();
	}

	// --- Tracks ---------------------------------------------------------------

	let trackRows = $state<Track[]>([]);
	$effect(() => {
		trackRows = draft?.tracks ?? [];
	});

	async function addTrack() {
		if (songIndex === null) return;
		const selected = await openDialog({
			multiple: false,
			filters: [{ name: 'Audio', extensions: ['wav'] }]
		});
		if (!selected || Array.isArray(selected)) return;
		const path = typeof selected === 'string' ? selected : (selected as { path: string }).path;
		const fileName = path.split(/[\\/]/).pop() ?? 'Track';
		const track = await api.addTrack(songIndex, path, fileName);
		project.update((p) => {
			if (!p || songIndex === null) return p;
			const songs = [...p.songs];
			songs[songIndex] = { ...songs[songIndex], tracks: [...songs[songIndex].tracks, track] };
			return { ...p, songs };
		});
		if (draft) draft.tracks = [...draft.tracks, track];
	}

	async function removeTrack(i: number) {
		if (songIndex === null) return;
		await api.removeTrack(songIndex, i);
		project.update((p) => {
			if (!p || songIndex === null) return p;
			const songs = [...p.songs];
			songs[songIndex] = {
				...songs[songIndex],
				tracks: songs[songIndex].tracks.filter((_, idx) => idx !== i)
			};
			return { ...p, songs };
		});
		if (draft) draft.tracks = draft.tracks.filter((_, idx) => idx !== i);
	}

	function handleTrackConsider(e: CustomEvent<{ items: Track[] }>) {
		trackRows = e.detail.items;
	}
	async function handleTrackFinalize(e: CustomEvent<{ items: Track[] }>) {
		trackRows = e.detail.items;
		if (!draft || songIndex === null || !song) return;
		const original = song.tracks;
		const newOrder = trackRows.map((t) => original.findIndex((o) => o.id === t.id));
		draft.tracks = trackRows;
		touch();
		try {
			await api.reorderTracks(songIndex, newOrder);
		} catch (err) {
			console.error('reorder_tracks failed', err);
		}
	}

	function trackGainInput(i: number, value: number) {
		if (!draft) return;
		draft.tracks[i].gain_db = value;
		touch();
		api.setTrackGain(i, value).catch(() => {});
	}
	function trackMutedInput(i: number, muted: boolean) {
		if (!draft) return;
		draft.tracks[i].muted = muted;
		touch();
		api.setTrackMuted(i, muted).catch(() => {});
	}
	function trackBusInput(i: number, bus: number) {
		if (!draft) return;
		draft.tracks[i].bus = bus;
		touch();
		api.setTrackBus(i, bus).catch(() => {});
	}

	// --- Offset nudge -----------------------------------------------------------

	function offsetMs(): number {
		return draft ? (draft.offset_samples / sampleRate) * 1000 : 0;
	}
	function setOffsetMs(ms: number) {
		if (!draft) return;
		draft.offset_samples = Math.round((ms / 1000) * sampleRate);
		touch();
	}
	function nudgeSamples(delta: number) {
		if (!draft) return;
		draft.offset_samples += delta;
		touch();
	}
	function nudgeMs(deltaMs: number) {
		setOffsetMs(offsetMs() + deltaMs);
	}
</script>

{#if draft}
	<div class="song-editor">
		<div class="row">
			<label class="title">
				<input
					type="text"
					bind:value={draft.title}
					oninput={touch}
					placeholder="Song title"
				/>
			</label>
			<label class="disabled-toggle">
				<input type="checkbox" bind:checked={draft.disabled} onchange={touch} />
				Disabled
			</label>
			<label class="disabled-toggle" title="At the end of this song, arm and play the next enabled song automatically, after the project's inter-song gap.">
				<input type="checkbox" bind:checked={draft.auto_continue} onchange={touch} />
				Auto-continue
			</label>
		</div>

		<div class="grid">
			<label>
				BPM
				<input type="number" step="0.1" bind:value={draft.bpm} oninput={touch} />
			</label>
			<label>
				Time sig
				<span class="ts">
					<input type="number" min="1" bind:value={draft.time_signature.numerator} oninput={touch} />
					/
					<input
						type="number"
						min="1"
						bind:value={draft.time_signature.denominator}
						oninput={touch}
					/>
				</span>
			</label>
			<label>
				Count-in (bars)
				<input type="number" min="0" max="4" bind:value={draft.count_in_bars} oninput={touch} />
			</label>
		</div>

		<div class="grid">
			<label>
				Offset (samples)
				<input type="number" bind:value={draft.offset_samples} oninput={touch} />
			</label>
			<label>
				Offset (ms)
				<input
					type="number"
					step="0.1"
					value={offsetMs().toFixed(2)}
					oninput={(e) => setOffsetMs(Number((e.target as HTMLInputElement).value))}
				/>
			</label>
			<div class="nudge">
				<button onclick={() => nudgeSamples(-1)}>-1 sample</button>
				<button onclick={() => nudgeSamples(1)}>+1 sample</button>
				<button onclick={() => nudgeMs(-1)}>-1 ms</button>
				<button onclick={() => nudgeMs(1)}>+1 ms</button>
			</div>
		</div>

		{#if saveError}
			<div class="error">{saveError}</div>
		{/if}
		{#each cueWarnings as w (w)}
			<div class="warning">Cue: {w}</div>
		{/each}

		<section>
			<div class="section-header">
				<h3>Sections</h3>
				<button onclick={addSection}>+ Add section</button>
			</div>
			<div class="section-table-header">
				<span></span>
				<span>Name</span>
				<span>Start bar</span>
				<span>Length</span>
				<span>Loop</span>
				<span>Cue text</span>
				<span>Cue lead</span>
				<span></span>
			</div>
			<div
				class="section-rows"
				use:dndzone={{ items: sectionRows, flipDurationMs: 150 }}
				onconsider={handleSectionConsider}
				onfinalize={handleSectionFinalize}
			>
				{#each sectionRows as row, i (row.dndId)}
					<div class="section-row">
						<span class="drag-handle">⠿</span>
						<input type="text" bind:value={row.section.name} oninput={touch} />
						<input type="number" min="1" bind:value={row.section.start_bar} oninput={touch} />
						<input type="number" min="1" bind:value={row.section.length_bars} oninput={touch} />
						<input type="checkbox" bind:checked={row.section.loopable} onchange={touch} />
						<input
							type="text"
							placeholder="(section name)"
							value={row.section.cue_text ?? ''}
							oninput={(e) => {
								row.section.cue_text = (e.target as HTMLInputElement).value || null;
								touch();
							}}
						/>
						<input type="number" min="0" bind:value={row.section.cue_lead_beats} oninput={touch} />
						<button class="row-remove" onclick={() => removeSection(i)}>✕</button>
					</div>
				{/each}
			</div>
		</section>

		<section>
			<div class="section-header">
				<h3>Tracks</h3>
				<button onclick={addTrack}>+ Add track…</button>
			</div>
			<div class="track-table-header">
				<span></span>
				<span>Name</span>
				<span>Gain (dB)</span>
				<span>Mute</span>
				<span>Bus</span>
				<span></span>
			</div>
			<div
				class="track-rows"
				use:dndzone={{ items: trackRows, flipDurationMs: 150 }}
				onconsider={handleTrackConsider}
				onfinalize={handleTrackFinalize}
			>
				{#each trackRows as track, i (track.id)}
					<div class="track-row">
						<span class="drag-handle">⠿</span>
						<span class="track-name">{track.name}</span>
						<input
							type="number"
							step="0.5"
							value={track.gain_db}
							oninput={(e) => trackGainInput(i, Number((e.target as HTMLInputElement).value))}
						/>
						<input
							type="checkbox"
							checked={track.muted}
							onchange={(e) => trackMutedInput(i, (e.target as HTMLInputElement).checked)}
						/>
						<input
							type="number"
							min="0"
							value={track.bus}
							oninput={(e) => trackBusInput(i, Number((e.target as HTMLInputElement).value))}
						/>
						<button class="row-remove" onclick={() => removeTrack(i)}>✕</button>
					</div>
				{/each}
			</div>
		</section>
	</div>
{:else}
	<div class="empty">Select or add a song to edit it.</div>
{/if}

<style>
	.song-editor {
		display: flex;
		flex-direction: column;
		gap: 1rem;
		flex: 1;
		min-width: 0;
	}
	.row {
		display: flex;
		align-items: center;
		gap: 1rem;
	}
	.title input {
		font-size: 1.3rem;
		font-weight: 700;
		background: none;
		border: none;
		border-bottom: 1px solid #2a2f36;
		color: inherit;
		padding: 0.25rem 0;
		min-width: 20rem;
	}
	.disabled-toggle {
		display: flex;
		align-items: center;
		gap: 0.3rem;
		color: #9fb3c8;
		font-size: 0.85rem;
	}
	.grid {
		display: flex;
		gap: 1.5rem;
		flex-wrap: wrap;
		align-items: flex-end;
	}
	.grid label {
		display: flex;
		flex-direction: column;
		gap: 0.25rem;
		font-size: 0.75rem;
		color: #9fb3c8;
		text-transform: uppercase;
		letter-spacing: 0.03em;
	}
	.grid input {
		background: #1b1f24;
		border: 1px solid #2a2f36;
		color: inherit;
		border-radius: 0.3rem;
		padding: 0.35rem 0.5rem;
		width: 6rem;
	}
	.ts {
		display: flex;
		align-items: center;
		gap: 0.3rem;
	}
	.ts input {
		width: 3.5rem;
	}
	.nudge {
		display: flex;
		gap: 0.3rem;
	}
	.error {
		color: #ff375f;
		font-size: 0.85rem;
	}
	.warning {
		color: #ffd60a;
		font-size: 0.85rem;
	}
	section h3 {
		margin: 0 0 0.5rem 0;
		font-size: 0.9rem;
		text-transform: uppercase;
		color: #9fb3c8;
		letter-spacing: 0.03em;
	}
	.section-header {
		display: flex;
		justify-content: space-between;
		align-items: center;
	}
	.section-table-header,
	.section-row {
		display: grid;
		grid-template-columns: 1.5rem 2fr 5rem 5rem 3.5rem 2fr 5rem 2rem;
		gap: 0.4rem;
		align-items: center;
	}
	.track-table-header,
	.track-row {
		display: grid;
		grid-template-columns: 1.5rem 2fr 6rem 3.5rem 4rem 2rem;
		gap: 0.4rem;
		align-items: center;
	}
	.section-table-header,
	.track-table-header {
		font-size: 0.7rem;
		text-transform: uppercase;
		color: #6d7f91;
		padding: 0 0.25rem;
	}
	.section-row,
	.track-row {
		background: #1b1f24;
		border: 1px solid #2a2f36;
		border-radius: 0.3rem;
		padding: 0.3rem;
		margin-bottom: 0.25rem;
	}
	.section-row input[type='text'],
	.section-row input[type='number'],
	.track-row input[type='number'] {
		background: #12151a;
		border: 1px solid #2a2f36;
		color: inherit;
		border-radius: 0.25rem;
		padding: 0.25rem 0.4rem;
		width: 100%;
	}
	.drag-handle {
		cursor: grab;
		color: #6d7f91;
		text-align: center;
	}
	.track-name {
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.row-remove {
		background: none;
		border: none;
		color: #6d7f91;
		cursor: pointer;
	}
	.row-remove:hover {
		color: #ff375f;
	}
	.empty {
		color: #6d7f91;
		padding: 2rem;
	}
</style>
