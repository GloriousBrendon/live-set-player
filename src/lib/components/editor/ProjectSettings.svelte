<script lang="ts">
	import { open as openDialog } from '@tauri-apps/plugin-dialog';
	import { project } from '$lib/stores/project';
	import { deviceStatus } from '$lib/stores/device';
	import * as api from '$lib/api';
	import { debounce } from '$lib/utils';
	import type { BusLayout, ClickConfig, CueConfig, VoiceInfo } from '$lib/types';
	import MidiSettings from './MidiSettings.svelte';

	let open = $state(false);
	let devices = $state<string[]>([]);
	let voices = $state<VoiceInfo[]>([]);

	async function refreshLists() {
		try {
			devices = await api.listOutputDevices();
		} catch {
			devices = [];
		}
		try {
			voices = await api.listVoices();
		} catch {
			voices = [];
		}
	}

	$effect(() => {
		if (open) refreshLists();
	});

	async function selectDevice(name: string) {
		try {
			await api.selectOutputDevice(name);
		} catch (err) {
			console.error('select_output_device failed', err);
		}
	}

	// Local draft of project-level (non-song) settings.
	let name = $state('');
	let click = $state<ClickConfig>({ bus: 1, gain_db: 0 });
	let cue = $state<CueConfig>({ voice_id: '', speed: 1, gain_db: 0, bus: 1 });
	let busLayout = $state<BusLayout>({ buses: [] });
	let lastProjectRef: unknown = null;

	$effect(() => {
		if ($project && $project !== lastProjectRef) {
			name = $project.name;
			click = { ...$project.click };
			cue = { ...$project.cue };
			busLayout = { buses: $project.bus_layout.buses.map((b) => ({ ...b })) };
			lastProjectRef = $project;
		}
	});

	const commit = debounce(async () => {
		try {
			const updated = await api.updateProjectSettings(name, click, cue, busLayout);
			lastProjectRef = updated;
			project.set(updated);
		} catch (err) {
			console.error('update_project_settings failed', err);
		}
	}, 600);

	async function addVoice() {
		const onnx = await openDialog({
			multiple: false,
			filters: [{ name: 'Piper voice', extensions: ['onnx'] }]
		});
		if (!onnx || Array.isArray(onnx)) return;
		const onnxPath = typeof onnx === 'string' ? onnx : (onnx as { path: string }).path;
		const id = onnxPath.split(/[\\/]/).pop()?.replace(/\.onnx$/, '') ?? 'voice';
		const configPath = `${onnxPath}.json`;
		try {
			await api.addVoiceFile(id, onnxPath, configPath);
			voices = await api.listVoices();
		} catch (err) {
			console.error('add_voice_file failed', err);
		}
	}
</script>

<div class="panel">
	<button class="toggle" onclick={() => (open = !open)}>{open ? '▾' : '▸'} Settings</button>
	{#if open}
		<div class="body">
			<div class="group">
				<h3>Project</h3>
				<label>
					Name
					<input type="text" bind:value={name} oninput={commit} />
				</label>
			</div>

			<div class="group">
				<h3>Output device</h3>
				<div class="device-status">
					{#if $deviceStatus}
						{$deviceStatus.configured_device ?? 'none selected'}
						{#if $deviceStatus.open}
							(open, {$deviceStatus.engine_rate} Hz)
						{/if}
					{/if}
				</div>
				<div class="device-list">
					{#each devices as d (d)}
						<button
							class:selected={$deviceStatus?.configured_device === d}
							onclick={() => selectDevice(d)}
						>
							{d}
						</button>
					{/each}
				</div>
			</div>

			<MidiSettings />

			<div class="group">
				<h3>Click</h3>
				<label>
					Bus
					<input type="number" min="0" bind:value={click.bus} oninput={commit} />
				</label>
				<label>
					Gain (dB)
					<input type="number" step="0.5" bind:value={click.gain_db} oninput={commit} />
				</label>
			</div>

			<div class="group">
				<h3>Cues</h3>
				<label>
					Voice
					<select bind:value={cue.voice_id} onchange={commit}>
						<option value="">(first available)</option>
						{#each voices as v (v.id)}
							<option value={v.id}>{v.id}</option>
						{/each}
					</select>
				</label>
				<button onclick={addVoice}>+ Add voice file…</button>
				<label>
					Speed
					<input
						type="number"
						step="0.05"
						min="0.5"
						max="2"
						bind:value={cue.speed}
						oninput={commit}
					/>
				</label>
				<label>
					Gain (dB)
					<input type="number" step="0.5" bind:value={cue.gain_db} oninput={commit} />
				</label>
				<label>
					Bus
					<input type="number" min="0" bind:value={cue.bus} oninput={commit} />
				</label>
			</div>

			<div class="group">
				<h3>Buses</h3>
				{#each busLayout.buses as bus, i (i)}
					<div class="bus-row">
						<input type="text" bind:value={bus.name} oninput={commit} />
						<input type="number" min="0" bind:value={bus.output_channel} oninput={commit} />
						<label>
							<input type="checkbox" bind:checked={bus.limiter_enabled} onchange={commit} />
							Limiter
						</label>
					</div>
				{/each}
			</div>
		</div>
	{/if}
</div>

<style>
	.panel {
		border: 1px solid #2a2f36;
		border-radius: 0.4rem;
		background: #14171b;
	}
	.toggle {
		width: 100%;
		text-align: left;
		background: none;
		border: none;
		color: #9fb3c8;
		padding: 0.5rem 0.75rem;
		cursor: pointer;
		font-size: 0.85rem;
		text-transform: uppercase;
		letter-spacing: 0.04em;
	}
	.body {
		display: flex;
		gap: 2rem;
		flex-wrap: wrap;
		padding: 0.5rem 0.75rem 1rem;
	}
	.group {
		display: flex;
		flex-direction: column;
		gap: 0.4rem;
		min-width: 10rem;
	}
	.group h3 {
		margin: 0;
		font-size: 0.7rem;
		text-transform: uppercase;
		color: #6d7f91;
	}
	.group label {
		display: flex;
		flex-direction: column;
		gap: 0.2rem;
		font-size: 0.75rem;
		color: #9fb3c8;
	}
	.group input,
	.group select {
		background: #1b1f24;
		border: 1px solid #2a2f36;
		color: inherit;
		border-radius: 0.3rem;
		padding: 0.3rem 0.4rem;
	}
	.device-list {
		display: flex;
		flex-direction: column;
		gap: 0.2rem;
	}
	.device-list button {
		text-align: left;
		background: #1b1f24;
		border: 1px solid #2a2f36;
		color: inherit;
		border-radius: 0.3rem;
		padding: 0.3rem 0.5rem;
		cursor: pointer;
	}
	.device-list button.selected {
		border-color: #3f8cff;
	}
	.bus-row {
		display: flex;
		gap: 0.4rem;
		align-items: center;
	}
</style>
