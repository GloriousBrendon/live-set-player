<script lang="ts">
	import { midiStatus } from '$lib/stores/midi';
	import * as api from '$lib/api';
	import type { Action, MidiBindingConfig, MidiMessage } from '$lib/types';

	let ports = $state<string[]>([]);
	let actions = $state<Action[]>([]);
	let error = $state<string | null>(null);

	async function refresh() {
		try {
			ports = await api.listMidiInputPorts();
		} catch {
			ports = [];
		}
		if (actions.length === 0) {
			try {
				actions = await api.listMidiActions();
			} catch {
				actions = [];
			}
		}
	}

	// Refreshed once on mount; the panel this lives in (ProjectSettings) is already
	// open by the time this component renders, so there's no separate "open" toggle
	// to key off like the output-device list has.
	refresh();

	const ACTION_LABELS: Record<Action, string> = {
		arm_next_song: 'Arm next song',
		play: 'Play',
		advance_section: 'Advance section',
		stop: 'Stop',
		panic_stop: 'Panic stop'
	};

	function formatMessage(m: MidiMessage): string {
		if ('note_on' in m) return `Note ${m.note_on.note} (ch ${m.note_on.channel + 1})`;
		if ('control_change' in m)
			return `CC ${m.control_change.controller} (ch ${m.control_change.channel + 1})`;
		return `Program ${m.program_change.program} (ch ${m.program_change.channel + 1})`;
	}

	function bindingFor(action: Action): MidiBindingConfig | undefined {
		return $midiStatus?.bindings.find((b) => b.action === action);
	}

	async function selectPort(name: string) {
		error = null;
		try {
			await api.selectMidiInputPort(name);
		} catch (err) {
			error = String(err);
		}
	}

	async function learn(action: Action) {
		error = null;
		try {
			await api.startMidiLearn(action);
		} catch (err) {
			error = String(err);
		}
	}

	async function cancelLearn() {
		await api.cancelMidiLearn().catch(() => {});
	}

	async function clearBinding(action: Action) {
		await api.removeMidiBinding(action).catch(() => {});
	}
</script>

<div class="group">
	<h3>MIDI input</h3>
	<div class="status">
		{#if $midiStatus}
			{$midiStatus.configured_port ?? 'none selected'}
			{#if $midiStatus.open}(open){/if}
		{/if}
	</div>
	{#if error}
		<div class="error">{error}</div>
	{/if}
	<div class="port-list">
		{#each ports as p (p)}
			<button
				class:selected={$midiStatus?.configured_port === p}
				onclick={() => selectPort(p)}
			>
				{p}
			</button>
		{/each}
		{#if ports.length === 0}
			<div class="empty">No MIDI input ports found.</div>
		{/if}
	</div>

	<div class="bindings">
		{#each actions as action (action)}
			{@const binding = bindingFor(action)}
			{@const learning = $midiStatus?.learn_pending === action}
			<div class="binding-row" class:learning>
				<span class="action-name">{ACTION_LABELS[action]}</span>
				<span class="binding-summary">
					{#if learning}
						Listening… press the pedal
					{:else if binding}
						{formatMessage(binding.message)}
					{:else}
						Unbound
					{/if}
				</span>
				{#if learning}
					<button onclick={cancelLearn}>Cancel</button>
				{:else}
					<button disabled={!$midiStatus?.open} onclick={() => learn(action)}>Learn</button>
					{#if binding}
						<button onclick={() => clearBinding(action)}>Clear</button>
					{/if}
				{/if}
			</div>
		{/each}
	</div>
</div>

<style>
	.group {
		display: flex;
		flex-direction: column;
		gap: 0.4rem;
		min-width: 16rem;
	}
	.group h3 {
		margin: 0;
		font-size: 0.7rem;
		text-transform: uppercase;
		color: #6d7f91;
	}
	.status {
		font-size: 0.8rem;
		color: #9fb3c8;
	}
	.error {
		color: #ff375f;
		font-size: 0.8rem;
	}
	.port-list {
		display: flex;
		flex-direction: column;
		gap: 0.2rem;
	}
	.port-list button {
		text-align: left;
		background: #1b1f24;
		border: 1px solid #2a2f36;
		color: inherit;
		border-radius: 0.3rem;
		padding: 0.3rem 0.5rem;
		cursor: pointer;
	}
	.port-list button.selected {
		border-color: #3f8cff;
	}
	.empty {
		font-size: 0.75rem;
		color: #6d7f91;
	}
	.bindings {
		display: flex;
		flex-direction: column;
		gap: 0.3rem;
		margin-top: 0.3rem;
	}
	.binding-row {
		display: flex;
		align-items: center;
		gap: 0.5rem;
		background: #1b1f24;
		border: 1px solid #2a2f36;
		border-radius: 0.3rem;
		padding: 0.3rem 0.5rem;
		font-size: 0.8rem;
	}
	.binding-row.learning {
		border-color: #3ddc84;
	}
	.action-name {
		flex: 1;
	}
	.binding-summary {
		color: #9fb3c8;
	}
	.binding-row button {
		background: none;
		border: 1px solid #2a2f36;
		color: inherit;
		border-radius: 0.3rem;
		padding: 0.2rem 0.5rem;
		cursor: pointer;
	}
	.binding-row button:disabled {
		opacity: 0.4;
		cursor: default;
	}
</style>
