import { writable } from 'svelte/store';
import type { Project, VerifyWarning } from '$lib/types';
import * as api from '$lib/api';

export const project = writable<Project | null>(null);
export const projectDir = writable<string | null>(null);
export const projectWarnings = writable<VerifyWarning[]>([]);
/** Index into `project.songs` currently selected in the editor / armed for playback. */
export const currentSongIndex = writable<number | null>(null);

export async function openProject(path: string): Promise<void> {
	const result = await api.loadProject(path);
	project.set(result.project);
	projectDir.set(path);
	projectWarnings.set(result.warnings);
	currentSongIndex.set(null);
}

export async function createProject(
	path: string,
	name: string,
	sampleRate: number
): Promise<void> {
	const p = await api.newProject(path, name, sampleRate);
	project.set(p);
	projectDir.set(path);
	projectWarnings.set([]);
	currentSongIndex.set(null);
}

export async function saveProject(): Promise<void> {
	await api.saveProject();
}
