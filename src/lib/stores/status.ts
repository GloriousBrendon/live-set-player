// Polls the engine status snapshot at ~30 Hz (docs/SPEC.md §9). `latest_status()` on
// the Rust side is sticky (returns the newest snapshot pushed by the audio callback,
// not every one) -- polling faster than the engine pushes would just re-read the same
// snapshot, so 33 ms matches the intended rate rather than exceeding it for no gain.

import { writable } from 'svelte/store';
import type { Status } from '$lib/types';
import { getStatus } from '$lib/api';

export const status = writable<Status | null>(null);

const POLL_MS = 33;
let timer: ReturnType<typeof setInterval> | undefined;

export function startStatusPolling(): void {
	if (timer) return;
	timer = setInterval(async () => {
		try {
			status.set(await getStatus());
		} catch {
			// No device open yet, or the command layer isn't reachable -- leave the
			// last known status in place rather than flashing a null status column.
		}
	}, POLL_MS);
}

export function stopStatusPolling(): void {
	if (timer) clearInterval(timer);
	timer = undefined;
}
