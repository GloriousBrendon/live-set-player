// Mirrors the serde DTOs exposed by the Tauri command layer
// (src-tauri/src/commands/*.rs, src-tauri/engine/src/{project,rt,loader,config}.rs).
// Keep field names identical to the Rust structs -- they cross the wire as-is.

export interface TimeSignature {
	numerator: number;
	denominator: number;
}

export interface Bus {
	name: string;
	output_channel: number;
	limiter_enabled: boolean;
}

export interface BusLayout {
	buses: Bus[];
}

export interface ClickConfig {
	bus: number;
	gain_db: number;
}

export interface CueConfig {
	voice_id: string;
	speed: number;
	gain_db: number;
	bus: number;
}

export interface AudioFileRef {
	path: string;
	sha256: string | null;
	frames: number | null;
}

export type DownmixMode = 'sum' | 'left' | 'right';
export type TrackKind = 'backtrack' | 'stem';

export interface Track {
	id: string;
	name: string;
	file: AudioFileRef;
	gain_db: number;
	muted: boolean;
	bus: number;
	downmix: DownmixMode;
	kind: TrackKind;
}

export interface Section {
	name: string;
	start_bar: number;
	length_bars: number;
	loopable: boolean;
	cue_text: string | null;
	cue_lead_beats: number;
}

export interface Song {
	id: string;
	title: string;
	bpm: number;
	time_signature: TimeSignature;
	offset_samples: number;
	count_in_bars: number;
	accent_pattern: number[];
	sections: Section[];
	tracks: Track[];
	disabled: boolean;
}

export interface Project {
	schema_version: number;
	name: string;
	sample_rate: number;
	bus_layout: BusLayout;
	click: ClickConfig;
	cue: CueConfig;
	songs: Song[];
}

export type VerifyWarningReason = 'missing' | 'hash_mismatch' | 'frame_count_mismatch';

export interface VerifyWarning {
	song_id: string;
	track_id: string;
	track_name: string;
	path: string;
	reason: VerifyWarningReason;
}

export interface LoadProjectResult {
	project: Project;
	warnings: VerifyWarning[];
}

export interface UpdateSongResult {
	song: Song;
	cue_warnings: string[];
}

// rt::Status / rt::TransportState / rt::QueuedStatus
export type TransportState = 'stopped' | 'playing' | 'stopping';
export type QueuedStatus = 'none' | { section: number } | 'end_of_song';

export interface Status {
	state: TransportState;
	perf_sample: number;
	section: number;
	queued: QueuedStatus;
	bars_remaining: number;
	count_in_beats_remaining: number | null;
	engine_rate: number;
	song_loaded: boolean;
	mmcss_pro_audio: boolean;
	xruns: number;
	callbacks: number;
}

export interface DeviceStatus {
	configured_device: string | null;
	open: boolean;
	engine_rate: number | null;
	channels: number | null;
	notice: string | null;
	error: number | null;
}

// lsp_engine::midi -- MIDI control input (docs/SPEC.md §9)
export type MidiMessage =
	| { note_on: { channel: number; note: number } }
	| { control_change: { channel: number; controller: number } }
	| { program_change: { channel: number; program: number } };

/** The five actions bindable to a MIDI trigger or a keyboard shortcut (§9). Also the
 *  argument to `dispatch_action`, the single code path both go through. */
export type Action = 'arm_next_song' | 'play' | 'advance_section' | 'stop' | 'panic_stop';

export interface MidiBindingConfig {
	message: MidiMessage;
	action: Action;
}

export interface MidiStatus {
	configured_port: string | null;
	open: boolean;
	bindings: MidiBindingConfig[];
	learn_pending: Action | null;
	last_learned: MidiBindingConfig | null;
}

export interface CueSyncSummary {
	rendered: number;
	cached: number;
	failed: string[];
}

export interface VoiceInfo {
	id: string;
}
