//! The real-time engine (`docs/SPEC.md` §3, §7): transport state machine, lock-free
//! UI↔audio communication, and the headless `process` entry point the cpal callback
//! (and the test suite) drives.
//!
//! Threading contract (CLAUDE.md invariant 1):
//!
//! - **UI → audio:** [`Command`]s over an `rtrb` SPSC queue.
//! - **Audio → UI:** [`Status`] snapshots over a second `rtrb` queue, one per
//!   `process` call, dropped when the queue is full (the UI polls at ~30 Hz and only
//!   ever wants the latest).
//! - **Freeing memory:** the audio thread never drops heap data. Replaced songs are
//!   pushed whole (`Box<Loaded>`) onto a garbage queue and dropped by whoever owns
//!   the [`GarbageDrain`] — a worker thread in the app, the test harness in tests.
//!   If the garbage queue is momentarily full the box parks in a retry slot on the
//!   engine and is re-offered next callback; it is never dropped in place.
//! - `process` allocates nothing, locks nothing, and does no I/O. Enforced by the
//!   counting-allocator test in `tests/rt_no_alloc.rs`.
//!
//! Transport semantics (§7 and the phase-2 scope):
//!
//! - Sections play in list order; `loopable` sections repeat until an advance or
//!   seek is queued.
//! - `AdvanceSection` / `SeekToSection` while playing quantise to the **next bar
//!   boundary**: the active entry is truncated there (exact grid arithmetic, see
//!   [`PlaybackCore::truncate_active_to_bar`]) and the queued target — visible in
//!   every [`Status`] until it lands — takes over at the boundary, through the §7
//!   crossfade when the splice is non-contiguous.
//! - `SeekToSection` while stopped arms the section: the next `Play` starts a fresh
//!   performance timeline there.
//! - `Play` from `Stopped` is preceded by a click-only count-in (§6) whenever the
//!   armed section's or the global override's count-in length is nonzero — this
//!   applies identically whether the armed section is bar 1 or a mid-song rehearsal
//!   point. See [`PlaybackCore::start`] and [`Command::SetCountInOverride`].
//! - `Stop` ramps out over ~10 ms; `PanicStop` over ~5 ms (invariant 5: even panic
//!   doesn't hard-cut, at gig timescales it is still immediate). Natural end of the
//!   last section lets the final click hit's decay tail ring out, then stops.
//!
//! Starvation is defined behaviour: the transport advances by exactly the frames
//! rendered and by nothing else, so skipped or late callbacks delay everything in
//! wall time but can never shift the click against the backtrack or corrupt state.
//! `tests/rt_equivalence.rs` pins this down.

use crate::click::{self, ClickSynthConfig};
use crate::core::{
    self, db_to_linear, CoreBus, CoreClick, CoreCue, CoreTrack, Entry, PlaybackCore, Sequencer,
    MAX_BLOCK_FRAMES,
};
use crate::cue_schedule;
use crate::error::RenderError;
use crate::project::{Project, Song};
use crate::render::{AudioBank, CueBank};
use crate::smoother::{default_ramp_samples, Smoother};
use crate::timeline::Grid;
use serde::Serialize;
use std::sync::Arc;

/// Stop ramp: 10 ms. Panic ramp: 5 ms. Both inside invariant 5's 5–10 ms window.
const STOP_RAMP_MS: f64 = 10.0;
const PANIC_RAMP_MS: f64 = 5.0;

/// Everything the audio thread needs to play one song, fully allocated off-thread
/// and handed over by `Box` through the command queue.
pub struct Loaded {
    pub core: PlaybackCore,
    pub meta: SongMeta,
}

pub struct SongMeta {
    pub grid: Grid,
    /// Song offset, already scaled to the engine rate.
    pub offset_samples: i64,
    pub sections: Vec<SectionInfo>,
    /// Preloaded cue clip per section, index-aligned with `sections`; `None` for a
    /// section with no cue (`docs/SPEC.md` §8). Populated once at prepare time from
    /// the already-rendered/cached WAVs `crate::tts` produces -- nothing here ever
    /// renders a cue, only schedules already-loaded audio.
    pub cue_clips: Vec<Option<Arc<[f32]>>>,
    /// Output channel per bus (from the project's `BusLayout`).
    pub bus_channels: Vec<u16>,
    /// This song's own count-in length (§6), already clamped to 0-4 bars. Used by
    /// `cmd_play` unless overridden — see [`Command::SetCountInOverride`].
    pub count_in_bars: u32,
    /// Exclusive performance-time end of the full section order, in samples (§9.1).
    /// Precomputed here, off the audio thread, straight from the grid — so the
    /// song-duration readout is `round(pulse * samples_per_pulse)` like every other
    /// position in the project, never a sum of section lengths (invariant 2).
    pub song_end_sample: i64,
    /// `loopable_at_or_after[i]` is true when section `i` or any section after it is
    /// `loopable`. Index-aligned with `sections`. A loopable section repeats an
    /// unknown number of times, so once one is in play or still ahead, the song's
    /// remaining time is genuinely unknowable and §9.1 requires reporting it as
    /// unknown rather than counting down to a boundary that will move. Precomputed
    /// so `push_status` is a single indexed read on the audio thread.
    pub loopable_at_or_after: Vec<bool>,
}

#[derive(Debug, Clone, Copy)]
pub struct SectionInfo {
    pub source_start_bar0: i64,
    pub length_bars: u32,
    pub loopable: bool,
    pub cue_lead_beats: u32,
}

/// Scale a sample count from one rate to another (used for `offset_samples` when the
/// engine rate differs from the project rate on Windows/WASAPI). Identity when the
/// rates match.
pub fn scale_samples(samples: i64, from_rate: u32, to_rate: u32) -> i64 {
    if from_rate == to_rate {
        samples
    } else {
        (samples as f64 * to_rate as f64 / from_rate as f64).round() as i64
    }
}

/// Build a [`Loaded`] for `song`, with every allocation done here (worker/UI
/// thread), never on the audio thread. `bank` must hold mono audio already
/// resampled to `engine_rate` (the loader's job). Validation mirrors
/// [`crate::render::render_song`].
pub fn prepare_loaded(
    project: &Project,
    song: &Song,
    bank: &AudioBank,
    cues: &CueBank,
    engine_rate: u32,
) -> Result<Box<Loaded>, RenderError> {
    let bus_count = project.bus_layout.buses.len().max(1);
    let click_bus = project.click.bus;
    if click_bus >= bus_count {
        return Err(RenderError::BusIndexOutOfRange(click_bus, bus_count));
    }
    let cue_bus = project.cue.bus;
    if cue_bus >= bus_count {
        return Err(RenderError::BusIndexOutOfRange(cue_bus, bus_count));
    }
    let grid = Grid::new(engine_rate, song.bpm, song.time_signature)?;
    let offset_samples = scale_samples(song.offset_samples, project.sample_rate, engine_rate);

    let mut tracks = Vec::with_capacity(song.tracks.len());
    for track in &song.tracks {
        if track.bus >= bus_count {
            return Err(RenderError::BusIndexOutOfRange(track.bus, bus_count));
        }
        let audio = bank
            .get(&track.id)
            .ok_or_else(|| RenderError::MissingTrack(track.id.clone()))?;
        tracks.push(CoreTrack {
            audio: audio.clone(),
            bus: track.bus,
            gain: Smoother::settled(db_to_linear(track.gain_db) as f32),
            mute: Smoother::settled(if track.muted { 0.0 } else { 1.0 }),
        });
    }

    let click = CoreClick {
        pattern: click::effective_accent_pattern(&song.accent_pattern, grid.pulses_per_bar()),
        cfg: ClickSynthConfig::default(),
        bus: click_bus,
        gain: Smoother::settled(db_to_linear(project.click.gain_db) as f32),
    };
    let (buses, bus_channels): (Vec<CoreBus>, Vec<u16>) = if project.bus_layout.buses.is_empty() {
        (
            vec![CoreBus {
                limiter_enabled: false,
            }],
            vec![0],
        )
    } else {
        project
            .bus_layout
            .buses
            .iter()
            .map(|b| {
                (
                    CoreBus {
                        limiter_enabled: b.limiter_enabled,
                    },
                    b.output_channel,
                )
            })
            .unzip()
    };

    let cue = CoreCue {
        bus: cue_bus,
        gain: Smoother::settled(db_to_linear(project.cue.gain_db) as f32),
    };
    let core = PlaybackCore::new(
        grid,
        offset_samples,
        tracks,
        click,
        cue,
        buses,
        song.sections.len(),
    );
    let sections = song
        .sections
        .iter()
        .map(|s| SectionInfo {
            source_start_bar0: (s.start_bar - 1) as i64,
            length_bars: s.length_bars,
            loopable: s.loopable,
            cue_lead_beats: s.cue_lead_beats,
        })
        .collect();
    let cue_clips = (0..song.sections.len())
        .map(|i| cues.get(i).cloned())
        .collect();

    // §9.1 song-length data, computed once here rather than on the audio thread.
    // Performance-time bars are the *sum of section lengths* (the order is the
    // performance order, §7), but the sample position of that end bar still goes
    // through the grid, so it rounds identically to every other event position.
    let total_perf_bars: i64 = song.sections.iter().map(|s| s.length_bars as i64).sum();
    let song_end_sample = grid.pulse_to_sample(grid.bar_to_pulse(total_perf_bars));
    let mut loopable_at_or_after = vec![false; song.sections.len()];
    let mut seen_loopable = false;
    for (i, sec) in song.sections.iter().enumerate().rev() {
        seen_loopable |= sec.loopable;
        loopable_at_or_after[i] = seen_loopable;
    }

    Ok(Box::new(Loaded {
        core,
        meta: SongMeta {
            grid,
            offset_samples,
            sections,
            cue_clips,
            bus_channels,
            count_in_bars: song.count_in_bars.min(4),
            song_end_sample,
            loopable_at_or_after,
        },
    }))
}

/// UI → audio commands. Everything here is processed at the start of a `process`
/// call, before any rendering, so command timing quantises to the block — and all
/// musical timing (advance boundaries) quantises to the bar grid from there.
pub enum Command {
    Play,
    Stop,
    PanicStop,
    /// While stopped: the next `Play` starts a fresh timeline at this section.
    ArmSection(usize),
    /// While playing: queued jump at the next bar boundary. While stopped: same as
    /// `ArmSection`.
    SeekToSection(usize),
    /// Queued jump to the next section in list order at the next bar boundary (or a
    /// stop at the boundary if the current section is the last).
    AdvanceSection,
    SetTrackGainDb {
        track: usize,
        db: f32,
    },
    SetTrackMuted {
        track: usize,
        muted: bool,
    },
    SetTrackBus {
        track: usize,
        bus: usize,
    },
    SetClickGainDb(f32),
    SetLimiterEnabled {
        bus: usize,
        enabled: bool,
    },
    /// Global count-in override (§6: "configurable ... overridable globally"):
    /// `None` uses each song's own `count_in_bars` (the default); `Some(n)` (clamped
    /// 0-4) overrides every subsequent `Play` regardless of song, until changed
    /// again. Persistent engine state, not a per-`Play` parameter, so a UI toggle
    /// (e.g. "rehearsal: no count-in") only needs to be set once.
    SetCountInOverride(Option<u32>),
    /// Swap in a new song (stops playback). The old song leaves via the garbage
    /// queue, never dropped on the audio thread.
    LoadSong(Box<Loaded>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TransportState {
    Stopped,
    Playing,
    /// Stop/panic ramp in progress; silence and `Stopped` follow within 5–10 ms.
    Stopping,
}

/// The queued-section indicator §7 requires the UI to show between an advance
/// trigger and the bar boundary where it lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum QueuedStatus {
    None,
    Section(u32),
    EndOfSong,
}

/// One audio→UI status snapshot. `Copy`, so it crosses the queue without touching
/// the heap. `Serialize` so the Tauri command layer can hand it to the frontend
/// as-is -- see `docs/SPEC.md` §9, "UI polls a status snapshot at ~30 Hz."
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Status {
    pub state: TransportState,
    /// Absolute performance-time sample position.
    pub perf_sample: i64,
    /// Index into the song's section list, -1 when nothing is active.
    pub section: i32,
    pub queued: QueuedStatus,
    /// Whole bars left in the active entry (counts down; 1 during the final bar).
    pub bars_remaining: u32,
    /// Seconds until the active entry's end (§9.1). Reflects a queued advance as soon
    /// as it is queued, because advancing rewrites the active entry's end. 0.0 when
    /// nothing is active.
    pub section_seconds_remaining: f64,
    /// Performance-time position in seconds. **Negative during a count-in** (§6),
    /// matching `perf_sample`; the UI shows its count-in indicator then.
    pub song_position_seconds: f64,
    /// Total length of the section order in seconds.
    pub song_duration_seconds: f64,
    /// Seconds until the end of the section order, or `None` when a `loopable`
    /// section is active or still ahead and the end is therefore not knowable (§9.1).
    pub song_seconds_remaining: Option<f64>,
    /// Pulses remaining until the count-in's target downbeat (§6); `Some`,
    /// decrementing to 0, only while a count-in is in progress, `None` otherwise
    /// (including once playback has started). The performance view's count-in
    /// indicator.
    pub count_in_beats_remaining: Option<u32>,
    pub engine_rate: u32,
    pub song_loaded: bool,
    /// Whether the callback thread is registered with MMCSS as "Pro Audio"
    /// (Windows only; always false elsewhere and in headless use).
    pub mmcss_pro_audio: bool,
    /// Callback-observed over/underrun count. cpal does not report xruns directly;
    /// this counts device-reported stream errors relayed by the error callback.
    pub xruns: u32,
    /// Total `process` invocations — a liveness counter: if this stops advancing
    /// while a stream is open, the device callback has stalled or died.
    pub callbacks: u32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Queued {
    None,
    Jump(usize),
    End,
}

pub enum Garbage {
    Loaded(Box<Loaded>),
}

/// Decide which cue(s) to push, if any, now that `entry` has become active
/// (`docs/SPEC.md` §8 live scheduling): pushes `entry`'s own cue unless it was
/// already prescheduled by its predecessor, and -- only when `entry`'s own section
/// isn't `loopable`, so the section after it is deterministic -- also proactively
/// schedules *that* section's cue with real lead time. A loopable current section
/// means the section after it can't be known until it's actually left, so nothing is
/// prescheduled there; that cue instead gets pushed with zero lead time (clamped to
/// "now" by [`cue_schedule::cue_start_and_end`]'s floor) the moment it actually
/// starts, via this same function.
///
/// Allocation-free: `out` is a scratch `Vec` the caller preallocates and reuses
/// (`RtEngine::cue_scratch` in live use); this function only ever `push`es into
/// existing capacity, matching CLAUDE.md invariant 1 for the live callers below.
/// `precomputed_cue_section` persists across calls (owned by the caller) so a
/// proactive push made for section N+1 while N was active is recognised and not
/// redundantly repeated when N+1 itself becomes active.
fn cues_to_push_for_activated_entry(
    meta: &SongMeta,
    entry: &Entry,
    precomputed_cue_section: &mut Option<usize>,
    out: &mut Vec<(i64, Arc<[f32]>)>,
) {
    let grid = meta.grid;
    let Some(section) = meta.sections.get(entry.section_index).copied() else {
        return;
    };

    if *precomputed_cue_section != Some(entry.section_index) {
        if let Some(Some(clip)) = meta.cue_clips.get(entry.section_index) {
            let (start, _end, _warning) = cue_schedule::cue_start_and_end(
                &grid,
                entry.perf_start_pulse,
                section.cue_lead_beats,
                clip.len() as i64,
                Some(entry.perf_start_sample),
            );
            out.push((start, clip.clone()));
        }
    }
    *precomputed_cue_section = None;

    if !section.loopable {
        let next_index = entry.section_index + 1;
        if let (Some(next_section), Some(Some(clip))) = (
            meta.sections.get(next_index).copied(),
            meta.cue_clips.get(next_index),
        ) {
            let next_entry = core::make_entry(
                &grid,
                meta.offset_samples,
                next_index,
                next_section.source_start_bar0,
                entry.perf_start_bar + entry.length_bars as i64,
                next_section.length_bars,
            );
            let (start, _end, _warning) = cue_schedule::cue_start_and_end(
                &grid,
                next_entry.perf_start_pulse,
                next_section.cue_lead_beats,
                clip.len() as i64,
                Some(entry.perf_start_sample),
            );
            out.push((start, clip.clone()));
            *precomputed_cue_section = Some(next_index);
        }
    }
}

/// Live-transport [`Sequencer`]: at each entry end, consume the queued jump if one
/// landed, else loop a loopable section, else fall through to the next section in
/// list order, else end. Pure arithmetic — allocation-free by construction.
struct TransportSeq<'a> {
    meta: &'a SongMeta,
    queued: &'a mut Queued,
    cue_pushes: &'a mut Vec<(i64, Arc<[f32]>)>,
    precomputed_cue_section: &'a mut Option<usize>,
}

impl Sequencer for TransportSeq<'_> {
    fn next_entry(&mut self, ended: &Entry) -> Option<Entry> {
        let target = match std::mem::replace(self.queued, Queued::None) {
            Queued::Jump(i) if i < self.meta.sections.len() => Some(i),
            Queued::Jump(_) => None,
            Queued::End => None,
            Queued::None => {
                if self
                    .meta
                    .sections
                    .get(ended.section_index)
                    .is_some_and(|s| s.loopable)
                {
                    Some(ended.section_index)
                } else if ended.section_index + 1 < self.meta.sections.len() {
                    Some(ended.section_index + 1)
                } else {
                    None
                }
            }
        };
        let next = target.map(|idx| {
            let s = &self.meta.sections[idx];
            core::make_entry(
                &self.meta.grid,
                self.meta.offset_samples,
                idx,
                s.source_start_bar0,
                ended.perf_start_bar + ended.length_bars as i64,
                s.length_bars,
            )
        });
        if let Some(next_entry) = &next {
            cues_to_push_for_activated_entry(
                self.meta,
                next_entry,
                self.precomputed_cue_section,
                self.cue_pushes,
            );
        }
        next
    }
}

/// The audio-thread half of the engine. Owned by (moved into) the cpal data
/// callback in live use; owned directly by tests in headless use.
pub struct RtEngine {
    cmd_rx: rtrb::Consumer<Command>,
    status_tx: rtrb::Producer<Status>,
    garbage_tx: rtrb::Producer<Garbage>,
    /// Retry slot for a swapped-out song that couldn't enter the garbage queue —
    /// parked here (no drop!) and re-offered every callback until it fits.
    garbage_retry: Option<Garbage>,
    loaded: Option<Box<Loaded>>,
    state: TransportState,
    queued: Queued,
    armed_section: usize,
    panic_pending: bool,
    /// Section whose cue has already been proactively pushed (with real lead time)
    /// while its predecessor was still active, awaiting that section actually
    /// becoming active -- see [`cues_to_push_for_activated_entry`]. `None` means no
    /// such prescheduling is outstanding (nothing pushed, predecessor was loopable,
    /// or it's already been consumed).
    precomputed_cue_section: Option<usize>,
    /// Scratch buffer for cue pushes decided during a `render_block` call (inside
    /// `TransportSeq`, which cannot reach `core` directly -- see module docs above)
    /// or directly in `cmd_play`. Preallocated once in [`new_engine`]; drained into
    /// `core.schedule_cue` calls and cleared every time it's used, never reallocated.
    cue_scratch: Vec<(i64, Arc<[f32]>)>,
    /// See [`Command::SetCountInOverride`].
    count_in_override: Option<u32>,
    engine_rate: u32,
    ramp_samples: u32,
    stop_ramp_samples: u32,
    panic_ramp_samples: u32,
    register_mmcss: bool,
    mmcss_attempted: bool,
    mmcss_ok: bool,
    xruns: u32,
    callbacks: u32,
}

/// UI-thread handle: send commands, poll status.
pub struct EngineHandle {
    cmd_tx: rtrb::Producer<Command>,
    status_rx: rtrb::Consumer<Status>,
    last_status: Option<Status>,
}

impl EngineHandle {
    /// Push a command; returns the command back if the queue is full (the caller
    /// may retry — the audio thread drains every callback, so a full queue clears
    /// within one buffer period).
    pub fn send(&mut self, cmd: Command) -> Result<(), Command> {
        self.cmd_tx.push(cmd).map_err(|rtrb::PushError::Full(c)| c)
    }

    /// Drain the status queue and return the most recent snapshot seen (sticky:
    /// keeps returning the last one when no new snapshot has arrived).
    pub fn latest_status(&mut self) -> Option<Status> {
        while let Ok(s) = self.status_rx.pop() {
            self.last_status = Some(s);
        }
        self.last_status
    }
}

/// Owner of the garbage queue's consuming end; `drain` drops whatever the audio
/// thread has discarded. Run it on a worker thread (or the test harness).
pub struct GarbageDrain {
    rx: rtrb::Consumer<Garbage>,
}

impl GarbageDrain {
    /// Drop everything currently queued; returns how many items were freed.
    pub fn drain(&mut self) -> usize {
        let mut n = 0;
        while self.rx.pop().is_ok() {
            n += 1;
        }
        n
    }
}

/// Build the engine triple. `register_mmcss` should be true only when the engine
/// will run inside a real device callback (see [`crate::mmcss`]).
pub fn new_engine(
    engine_rate: u32,
    register_mmcss: bool,
) -> (RtEngine, EngineHandle, GarbageDrain) {
    let (cmd_tx, cmd_rx) = rtrb::RingBuffer::new(256);
    let (status_tx, status_rx) = rtrb::RingBuffer::new(64);
    let (garbage_tx, garbage_rx) = rtrb::RingBuffer::new(16);
    let engine = RtEngine {
        cmd_rx,
        status_tx,
        garbage_tx,
        garbage_retry: None,
        loaded: None,
        state: TransportState::Stopped,
        queued: Queued::None,
        armed_section: 0,
        panic_pending: false,
        precomputed_cue_section: None,
        // At most two pushes per transition (the landed section's own cue plus a
        // proactive push for the one after it); a handful of spare slots covers
        // back-to-back transitions inside a single `render_block` call without ever
        // needing to grow on the audio thread.
        cue_scratch: Vec::with_capacity(8),
        count_in_override: None,
        engine_rate,
        ramp_samples: default_ramp_samples(engine_rate),
        stop_ramp_samples: (STOP_RAMP_MS / 1000.0 * engine_rate as f64).round() as u32,
        panic_ramp_samples: (PANIC_RAMP_MS / 1000.0 * engine_rate as f64).round() as u32,
        register_mmcss,
        mmcss_attempted: false,
        mmcss_ok: false,
        xruns: 0,
        callbacks: 0,
    };
    let handle = EngineHandle {
        cmd_tx,
        status_rx,
        last_status: None,
    };
    (engine, handle, GarbageDrain { rx: garbage_rx })
}

impl RtEngine {
    pub fn engine_rate(&self) -> u32 {
        self.engine_rate
    }

    /// Record a device-reported stream error (called from the cpal error callback's
    /// sibling state, relayed as a count).
    pub fn note_xrun(&mut self) {
        self.xruns = self.xruns.saturating_add(1);
    }

    /// Render `out.len() / channels` frames of interleaved output. This is the whole
    /// audio callback: MMCSS registration on first entry (live only), command drain,
    /// block rendering through the shared [`PlaybackCore`], bus→channel mapping,
    /// one status push. No allocation, locks, I/O, or heap drops anywhere below.
    pub fn process(&mut self, out: &mut [f32], channels: usize) {
        self.callbacks = self.callbacks.wrapping_add(1);
        if self.register_mmcss && !self.mmcss_attempted {
            self.mmcss_attempted = true;
            self.mmcss_ok = crate::mmcss::register_current_thread_pro_audio();
        }
        self.retry_garbage();
        self.drain_commands();

        out.fill(0.0);
        if channels == 0 {
            return;
        }
        let frames = out.len() / channels;

        if matches!(
            self.state,
            TransportState::Playing | TransportState::Stopping
        ) {
            if let Some(loaded) = self.loaded.as_mut() {
                let mut done = 0usize;
                while done < frames {
                    let n = (frames - done).min(MAX_BLOCK_FRAMES);
                    {
                        let Loaded { core, meta } = loaded.as_mut();
                        let mut seq = TransportSeq {
                            meta,
                            queued: &mut self.queued,
                            cue_pushes: &mut self.cue_scratch,
                            precomputed_cue_section: &mut self.precomputed_cue_section,
                        };
                        core.render_block(n, &mut seq);
                    }
                    if !self.cue_scratch.is_empty() {
                        for (start, clip) in self.cue_scratch.drain(..) {
                            loaded.core.schedule_cue(start, clip);
                        }
                    }
                    let core = &loaded.core;
                    for bus in 0..core.bus_count() {
                        let ch = loaded
                            .meta
                            .bus_channels
                            .get(bus)
                            .copied()
                            .unwrap_or(u16::MAX) as usize;
                        if ch >= channels {
                            continue; // bus mapped past the device's channel count
                        }
                        let src = core.bus_buffer(bus);
                        for (i, &s) in src.iter().enumerate().take(n) {
                            out[(done + i) * channels + ch] += s;
                        }
                    }
                    done += n;
                }

                // Post-render transport transitions.
                match self.state {
                    TransportState::Stopping => {
                        if loaded.core.master_settled_at(0.0) {
                            loaded.core.clear_active();
                            self.queued = Queued::None;
                            self.panic_pending = false;
                            self.state = TransportState::Stopped;
                        }
                    }
                    TransportState::Playing => {
                        // `active` is also `None` while a count-in is in progress
                        // (see `PlaybackCore::start`/`pending`) — without the
                        // `pending().is_none()` check here, the transport would
                        // mistake "counting in" for "song has ended" and stop itself
                        // the instant Play is pressed.
                        if loaded.core.active().is_none() && loaded.core.pending().is_none() {
                            // Natural end: keep rendering until the final click
                            // hit's decay tail has fully rung out, then stop. Exact
                            // sample accounting, not block-granular — the offline
                            // render's tail must not be truncated by callback size.
                            let content_end = loaded
                                .meta
                                .grid
                                .pulse_to_sample(loaded.core.schedule_end_pulse());
                            if loaded.core.perf_pos() >= content_end + loaded.core.hit_length() {
                                self.queued = Queued::None;
                                self.state = TransportState::Stopped;
                            }
                        }
                    }
                    TransportState::Stopped => {}
                }
            } else {
                self.state = TransportState::Stopped;
            }
        }

        self.push_status();
    }

    fn drain_commands(&mut self) {
        while let Ok(cmd) = self.cmd_rx.pop() {
            self.apply_command(cmd);
        }
    }

    fn apply_command(&mut self, cmd: Command) {
        match cmd {
            Command::Play => self.cmd_play(),
            Command::Stop => {
                if self.state == TransportState::Playing {
                    if let Some(loaded) = self.loaded.as_mut() {
                        loaded.core.set_master(0.0, self.stop_ramp_samples);
                        self.state = TransportState::Stopping;
                    }
                }
            }
            Command::PanicStop => {
                if matches!(
                    self.state,
                    TransportState::Playing | TransportState::Stopping
                ) {
                    if let Some(loaded) = self.loaded.as_mut() {
                        loaded.core.set_master(0.0, self.panic_ramp_samples);
                        self.queued = Queued::None;
                        self.panic_pending = true;
                        self.state = TransportState::Stopping;
                    }
                }
            }
            Command::ArmSection(i) => self.armed_section = i,
            Command::SeekToSection(i) => {
                if self.state == TransportState::Playing {
                    self.queue_jump(Queued::Jump(i));
                } else {
                    self.armed_section = i;
                }
            }
            Command::AdvanceSection => {
                if self.state == TransportState::Playing {
                    let target = self
                        .loaded
                        .as_ref()
                        .and_then(|l| l.core.active())
                        .map(|a| a.section_index + 1);
                    if let (Some(t), Some(count)) =
                        (target, self.loaded.as_ref().map(|l| l.meta.sections.len()))
                    {
                        let q = if t < count {
                            Queued::Jump(t)
                        } else {
                            Queued::End
                        };
                        self.queue_jump(q);
                    }
                }
            }
            Command::SetTrackGainDb { track, db } => {
                if let Some(l) = self.loaded.as_mut() {
                    l.core
                        .set_track_gain(track, db_to_linear(db as f64) as f32, self.ramp_samples);
                }
            }
            Command::SetTrackMuted { track, muted } => {
                if let Some(l) = self.loaded.as_mut() {
                    l.core.set_track_muted(track, muted, self.ramp_samples);
                }
            }
            Command::SetTrackBus { track, bus } => {
                if let Some(l) = self.loaded.as_mut() {
                    l.core.set_track_bus(track, bus);
                }
            }
            Command::SetClickGainDb(db) => {
                if let Some(l) = self.loaded.as_mut() {
                    l.core
                        .set_click_gain(db_to_linear(db as f64) as f32, self.ramp_samples);
                }
            }
            Command::SetLimiterEnabled { bus, enabled } => {
                if let Some(l) = self.loaded.as_mut() {
                    l.core.set_limiter_enabled(bus, enabled);
                }
            }
            Command::SetCountInOverride(bars) => {
                self.count_in_override = bars.map(|b| b.min(4));
            }
            Command::LoadSong(new) => {
                let old = self.loaded.replace(new);
                self.state = TransportState::Stopped;
                self.queued = Queued::None;
                self.armed_section = 0;
                self.panic_pending = false;
                self.precomputed_cue_section = None;
                if let Some(old) = old {
                    self.discard(Garbage::Loaded(old));
                }
            }
        }
    }

    fn cmd_play(&mut self) {
        if self.state != TransportState::Stopped {
            return;
        }
        let armed = self.armed_section;
        let Some(loaded) = self.loaded.as_mut() else {
            return;
        };
        let Some(section) = loaded.meta.sections.get(armed).copied() else {
            return;
        };
        let first = core::make_entry(
            &loaded.meta.grid,
            loaded.meta.offset_samples,
            armed,
            section.source_start_bar0,
            0,
            section.length_bars,
        );
        // §6: count-in applies whichever section is armed (this generic "any armed
        // section" path is also how a mid-song rehearsal start works), and the
        // global override (if set) takes precedence over the song's own default.
        let count_in_bars = self
            .count_in_override
            .unwrap_or(loaded.meta.count_in_bars)
            .min(4);
        // Instant master reset is a start from silence, not a live gain change —
        // the one case invariant 5 permits a zero-length ramp.
        loaded.core.set_master(1.0, 0);
        loaded.core.start(first, count_in_bars);
        self.queued = Queued::None;
        self.state = TransportState::Playing;

        // A fresh `Play` can land anywhere (armed by `ArmSection`/`SeekToSection`
        // while stopped, per §7's "the next `Play` starts a fresh timeline there"),
        // so any prescheduling left over from a previous play-through no longer
        // applies -- start clean, then schedule the first entry's own cue exactly
        // like any other activation (see `cues_to_push_for_activated_entry`).
        self.precomputed_cue_section = None;
        cues_to_push_for_activated_entry(
            &loaded.meta,
            &first,
            &mut self.precomputed_cue_section,
            &mut self.cue_scratch,
        );
        for (start, clip) in self.cue_scratch.drain(..) {
            loaded.core.schedule_cue(start, clip);
        }
    }

    /// Queue a jump and truncate the active entry at the next bar boundary
    /// (strictly after the current position). If the boundary falls at or past the
    /// entry's natural end, the entry is left alone and the queued jump simply
    /// takes over at that end — which is itself a bar boundary.
    fn queue_jump(&mut self, q: Queued) {
        let Some(loaded) = self.loaded.as_mut() else {
            return;
        };
        if loaded.core.active().is_none() {
            return;
        }
        let grid = loaded.meta.grid;
        let ppb = grid.pulses_per_bar();
        let current_pulse = grid.sample_to_pulse(loaded.core.perf_pos());
        let boundary_bar = current_pulse.div_euclid(ppb) + 1;
        loaded.core.truncate_active_to_bar(boundary_bar);
        self.queued = q;
    }

    fn discard(&mut self, garbage: Garbage) {
        debug_assert!(self.garbage_retry.is_none());
        if let Err(rtrb::PushError::Full(g)) = self.garbage_tx.push(garbage) {
            // Park it; never drop on the audio thread. Re-offered next callback.
            self.garbage_retry = Some(g);
        }
    }

    fn retry_garbage(&mut self) {
        if let Some(g) = self.garbage_retry.take() {
            if let Err(rtrb::PushError::Full(g)) = self.garbage_tx.push(g) {
                self.garbage_retry = Some(g);
            }
        }
    }

    fn push_status(&mut self) {
        let secs = |samples: i64| samples as f64 / self.engine_rate as f64;
        let (section, bars_remaining, section_seconds_remaining, perf_sample, song_loaded) =
            match self.loaded.as_ref() {
                Some(l) => {
                    let perf = l.core.perf_pos();
                    match l.core.active() {
                        Some(a) => {
                            let ppb = l.meta.grid.pulses_per_bar();
                            let remaining_pulses =
                                (a.perf_end_pulse - l.meta.grid.sample_to_pulse(perf)).max(0);
                            let bars = ((remaining_pulses + ppb - 1) / ppb).max(0) as u32;
                            // §9.1: from the entry's end *sample*, not its bar count,
                            // so the readout stays smooth within the final bar. An
                            // advance rewrites `perf_end_sample`, so a queued advance
                            // shortens this the moment it is queued.
                            let secs_left = secs((a.perf_end_sample - perf).max(0));
                            (a.section_index as i32, bars, secs_left, perf, true)
                        }
                        None => (-1, 0, 0.0, perf, true),
                    }
                }
                None => (-1, 0, 0.0, 0, false),
            };
        // §9.1 song-level time. `song_seconds_remaining` is `None` whenever a
        // loopable section is active or still ahead: its repeat count isn't knowable,
        // so any number here would be a lie the UI would display in large type.
        let (song_position_seconds, song_duration_seconds, song_seconds_remaining) =
            match self.loaded.as_ref() {
                Some(l) => {
                    let unknown_end = section >= 0
                        && l.meta
                            .loopable_at_or_after
                            .get(section as usize)
                            .copied()
                            .unwrap_or(false);
                    let remaining =
                        (!unknown_end).then(|| secs((l.meta.song_end_sample - perf_sample).max(0)));
                    (secs(perf_sample), secs(l.meta.song_end_sample), remaining)
                }
                None => (0.0, 0.0, None),
            };
        // §6: pulses remaining until the count-in's target downbeat, read from the
        // grid (never a separately-maintained counter) so it can't drift from the
        // click that's actually sounding.
        let count_in_beats_remaining = self.loaded.as_ref().and_then(|l| {
            l.core.pending().map(|p| {
                let current_pulse = l.meta.grid.sample_to_pulse(l.core.perf_pos());
                (p.perf_start_pulse - current_pulse).max(0) as u32
            })
        });
        let queued = match self.queued {
            Queued::None => QueuedStatus::None,
            Queued::Jump(i) => QueuedStatus::Section(i as u32),
            Queued::End => QueuedStatus::EndOfSong,
        };
        let status = Status {
            state: self.state,
            perf_sample,
            section,
            queued,
            bars_remaining,
            section_seconds_remaining,
            song_position_seconds,
            song_duration_seconds,
            song_seconds_remaining,
            count_in_beats_remaining,
            engine_rate: self.engine_rate,
            song_loaded,
            mmcss_pro_audio: self.mmcss_ok,
            xruns: self.xruns,
            callbacks: self.callbacks,
        };
        let _ = self.status_tx.push(status);
    }
}
