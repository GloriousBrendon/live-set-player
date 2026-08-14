// Typed wrappers around the Tauri command layer (src-tauri/src/commands/*.rs). One
// function per command; keep it a flat pass-through, no logic here.

import { invoke } from '@tauri-apps/api/core';
import type {
	BusLayout,
	ClickConfig,
	CueConfig,
	CueSyncSummary,
	DeviceStatus,
	LoadProjectResult,
	Project,
	Song,
	Status,
	Track,
	UpdateSongResult,
	VoiceInfo
} from './types';

// Transport (docs/SPEC.md §7, §9)
export const getStatus = () => invoke<Status | null>('get_status');
export const play = () => invoke<void>('play');
export const stop = () => invoke<void>('stop');
export const panicStop = () => invoke<void>('panic_stop');
export const armSection = (section: number) => invoke<void>('arm_section', { section });
export const seekToSection = (section: number) => invoke<void>('seek_to_section', { section });
export const advanceSection = () => invoke<void>('advance_section');
export const setTrackGain = (track: number, db: number) =>
	invoke<void>('set_track_gain', { track, db });
export const setTrackMuted = (track: number, muted: boolean) =>
	invoke<void>('set_track_muted', { track, muted });
export const setTrackBus = (track: number, bus: number) =>
	invoke<void>('set_track_bus', { track, bus });
export const setClickGain = (db: number) => invoke<void>('set_click_gain', { db });
export const setLimiterEnabled = (bus: number, enabled: boolean) =>
	invoke<void>('set_limiter_enabled', { bus, enabled });
export const setCountInOverride = (bars: number | null) =>
	invoke<void>('set_count_in_override', { bars });
export const armSong = (songId: string) => invoke<Song>('arm_song', { songId });
export const armNextSong = () => invoke<Song | null>('arm_next_song');

// Project (docs/SPEC.md §7, §11)
export const loadProject = (path: string) => invoke<LoadProjectResult>('load_project', { path });
export const saveProject = () => invoke<void>('save_project');
export const newProject = (path: string, name: string, sampleRate: number) =>
	invoke<Project>('new_project', { path, name, sampleRate });
export const getProject = () => invoke<Project | null>('get_project');
export const updateProjectSettings = (
	name: string,
	click: ClickConfig,
	cue: CueConfig,
	busLayout: BusLayout
) => invoke<Project>('update_project_settings', { name, click, cue, busLayout });
export const addSong = () => invoke<Song>('add_song');
export const removeSong = (songIndex: number) => invoke<void>('remove_song', { songIndex });
export const reorderSongs = (newOrder: number[]) =>
	invoke<void>('reorder_songs', { newOrder });
export const updateSong = (songIndex: number, song: Song) =>
	invoke<UpdateSongResult>('update_song', { songIndex, song });
export const addTrack = (songIndex: number, sourcePath: string, name: string) =>
	invoke<Track>('add_track', { songIndex, sourcePath, name });
export const removeTrack = (songIndex: number, trackIndex: number) =>
	invoke<void>('remove_track', { songIndex, trackIndex });
export const reorderTracks = (songIndex: number, newOrder: number[]) =>
	invoke<void>('reorder_tracks', { songIndex, newOrder });

// Device (docs/SPEC.md §1)
export const listOutputDevices = () => invoke<string[]>('list_output_devices');
export const selectOutputDevice = (name: string) =>
	invoke<DeviceStatus>('select_output_device', { name });
export const getDeviceStatus = () => invoke<DeviceStatus>('get_device_status');

// Cues / voices (docs/SPEC.md §8)
export const syncAllCues = () => invoke<CueSyncSummary>('sync_all_cues');
export const listVoices = () => invoke<VoiceInfo[]>('list_voices');
export const addVoiceFile = (id: string, onnxPath: string, configPath: string) =>
	invoke<void>('add_voice_file', { id, onnxPath, configPath });
