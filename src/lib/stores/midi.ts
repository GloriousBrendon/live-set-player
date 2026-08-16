import { writable } from 'svelte/store';
import type { MidiStatus } from '$lib/types';
import { getMidiStatus } from '$lib/api';

export const midiStatus = writable<MidiStatus | null>(null);

// Mirrors stores/device.ts's poll-rate-matches-volatility precedent: MIDI status
// changes rarely at rest (open/close, bindings) but a learn capture should feel
// responsive, so poll faster only while learn mode is actually pending.
const IDLE_POLL_MS = 1000;
const LEARN_POLL_MS = 150;
let timer: ReturnType<typeof setInterval> | undefined;
let currentIntervalMs: number | undefined;

function reschedule(intervalMs: number): void {
	if (currentIntervalMs === intervalMs) return;
	if (timer) clearInterval(timer);
	currentIntervalMs = intervalMs;
	timer = setInterval(poll, intervalMs);
}

async function poll(): Promise<void> {
	try {
		const s = await getMidiStatus();
		midiStatus.set(s);
		reschedule(s.learn_pending ? LEARN_POLL_MS : IDLE_POLL_MS);
	} catch {
		// ignore transient IPC failures
	}
}

export function startMidiStatusPolling(): void {
	if (timer) return;
	poll();
	currentIntervalMs = IDLE_POLL_MS;
	timer = setInterval(poll, IDLE_POLL_MS);
}

export function stopMidiStatusPolling(): void {
	if (timer) clearInterval(timer);
	timer = undefined;
	currentIntervalMs = undefined;
}
