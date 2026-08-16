// Typed wrappers around the Tauri command layer (src-tauri/src/commands/*.rs). One
// function per command; keep it a flat pass-through, no logic here.

import { invoke } from '@tauri-apps/api/core';
import type {
	Action,
	BusLayout,
	ClickConfig,
	CueConfig,
	CueSyncSummary,
	DeviceStatus,
	LoadProjectResult,
	MidiStatus,
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
/** The single code path both keyboard shortcuts and MIDI bindings dispatch through. */
export const dispatchAction = (action: Action) => invoke<void>('dispatch_action', { action });
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
export const duplicateSong = (songIndex: number) => invoke<Song>('duplicate_song', { songIndex });
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

// MIDI input (docs/SPEC.md §9)
export const listMidiInputPorts = () => invoke<string[]>('list_midi_input_ports');
export const selectMidiInputPort = (name: string) =>
	invoke<MidiStatus>('select_midi_input_port', { name });
export const getMidiStatus = () => invoke<MidiStatus>('get_midi_status');
export const startMidiLearn = (action: Action) => invoke<void>('start_midi_learn', { action });
export const cancelMidiLearn = () => invoke<void>('cancel_midi_learn');
export const removeMidiBinding = (action: Action) =>
	invoke<MidiStatus>('remove_midi_binding', { action });
export const listMidiActions = () => invoke<Action[]>('list_midi_actions');

// Cues / voices (docs/SPEC.md §8)
export const syncAllCues = () => invoke<CueSyncSummary>('sync_all_cues');
export const listVoices = () => invoke<VoiceInfo[]>('list_voices');
export const addVoiceFile = (id: string, onnxPath: string, configPath: string) =>
	invoke<void>('add_voice_file', { id, onnxPath, configPath });
