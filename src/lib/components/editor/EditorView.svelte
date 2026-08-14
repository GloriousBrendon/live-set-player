<script lang="ts">
	import { open as openDialog } from '@tauri-apps/plugin-dialog';
	import {
		project,
		projectDir,
		projectWarnings,
		createProject,
		openProject,
		saveProject
	} from '$lib/stores/project';
	import SongList from './SongList.svelte';
	import SongEditor from './SongEditor.svelte';
	import ProjectSettings from './ProjectSettings.svelte';

	let showNewForm = $state(false);
	let newName = $state('New Set');
	let newDir = $state<string | null>(null);
	let newSampleRate = $state(48000);
	let busy = $state(false);
	let errorMsg = $state<string | null>(null);

	async function handleOpen() {
		const dir = await openDialog({ directory: true, multiple: false });
		if (!dir || Array.isArray(dir)) return;
		busy = true;
		errorMsg = null;
		try {
			await openProject(dir);
		} catch (err) {
			errorMsg = String(err);
		} finally {
			busy = false;
		}
	}

	async function pickNewParent() {
		const dir = await openDialog({ directory: true, multiple: false });
		if (dir && !Array.isArray(dir)) newDir = dir;
	}

	async function confirmNew() {
		if (!newDir || !newName.trim()) return;
		const path = `${newDir.replace(/[\\/]+$/, '')}/${newName.trim()}.lsp`;
		busy = true;
		errorMsg = null;
		try {
			await createProject(path, newName.trim(), newSampleRate);
			showNewForm = false;
		} catch (err) {
			errorMsg = String(err);
		} finally {
			busy = false;
		}
	}

	async function handleSave() {
		busy = true;
		errorMsg = null;
		try {
			await saveProject();
		} catch (err) {
			errorMsg = String(err);
		} finally {
			busy = false;
		}
	}
</script>

<div class="editor">
	<div class="toolbar">
		<button onclick={handleOpen} disabled={busy}>Open project…</button>
		<button onclick={() => (showNewForm = !showNewForm)} disabled={busy}>New project…</button>
		<button onclick={handleSave} disabled={busy || !$project}>Save (Ctrl+S)</button>
		{#if $projectDir}<span class="dir">{$projectDir}</span>{/if}
	</div>

	{#if showNewForm}
		<div class="new-form">
			<input type="text" bind:value={newName} placeholder="Set name" />
			<button onclick={pickNewParent}>{newDir ?? 'Choose folder…'}</button>
			<select bind:value={newSampleRate}>
				<option value={44100}>44100 Hz</option>
				<option value={48000}>48000 Hz</option>
			</select>
			<button onclick={confirmNew} disabled={!newDir}>Create</button>
		</div>
	{/if}

	{#if errorMsg}
		<div class="error">{errorMsg}</div>
	{/if}

	{#if $projectWarnings.length}
		<div class="warnings">
			{#each $projectWarnings as w (w.track_id + w.path)}
				<div>⚠ {w.track_name} ({w.path}): {w.reason.replaceAll('_', ' ')}</div>
			{/each}
		</div>
	{/if}

	{#if $project}
		<ProjectSettings />
		<div class="workspace">
			<SongList />
			<SongEditor />
		</div>
	{:else}
		<div class="empty">Open or create a project to get started.</div>
	{/if}
</div>

<style>
	.editor {
		display: flex;
		flex-direction: column;
		gap: 1rem;
		padding: 1rem;
		height: 100vh;
		box-sizing: border-box;
		overflow-y: auto;
		background: #0d0f12;
		color: #e6edf3;
	}
	.toolbar {
		display: flex;
		align-items: center;
		gap: 0.5rem;
	}
	.toolbar .dir {
		color: #6d7f91;
		font-size: 0.8rem;
		margin-left: 1rem;
	}
	.new-form {
		display: flex;
		gap: 0.5rem;
		align-items: center;
		background: #14171b;
		border: 1px solid #2a2f36;
		border-radius: 0.4rem;
		padding: 0.5rem;
	}
	.error {
		color: #ff375f;
	}
	.warnings {
		color: #ffd60a;
		font-size: 0.85rem;
		display: flex;
		flex-direction: column;
		gap: 0.15rem;
	}
	.workspace {
		display: flex;
		gap: 1.5rem;
		flex: 1;
		min-height: 0;
	}
	.empty {
		color: #6d7f91;
		padding: 3rem;
		text-align: center;
	}

	button {
		background: #1b1f24;
		border: 1px solid #2a2f36;
		color: inherit;
		border-radius: 0.3rem;
		padding: 0.4rem 0.7rem;
		cursor: pointer;
		font-size: 0.85rem;
	}
	button:hover:not(:disabled) {
		border-color: #3f8cff;
	}
	button:disabled {
		opacity: 0.5;
		cursor: default;
	}
	input,
	select {
		background: #1b1f24;
		border: 1px solid #2a2f36;
		color: inherit;
		border-radius: 0.3rem;
		padding: 0.4rem 0.5rem;
	}
</style>
