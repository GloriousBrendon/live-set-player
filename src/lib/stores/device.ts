import { writable } from 'svelte/store';
import type { DeviceStatus } from '$lib/types';
import { getDeviceStatus } from '$lib/api';

export const deviceStatus = writable<DeviceStatus | null>(null);

// Device status changes rarely (open/close, error flag) -- 1s is plenty, unlike the
// transport status poll which tracks audio in near-real-time.
const POLL_MS = 1000;
let timer: ReturnType<typeof setInterval> | undefined;

export function startDeviceStatusPolling(): void {
	if (timer) return;
	const poll = async () => {
		try {
			deviceStatus.set(await getDeviceStatus());
		} catch {
			// ignore transient IPC failures
		}
	};
	poll();
	timer = setInterval(poll, POLL_MS);
}

export function stopDeviceStatusPolling(): void {
	if (timer) clearInterval(timer);
	timer = undefined;
}
