//! Offline render mode (`docs/SPEC.md` §12).
//!
//! The headless renderer: given a project, one of its songs, and a performance order,
//! produce an interleaved stereo `f32` buffer -- bus 0 to the left channel, bus 1 to
//! the right -- and write it to a WAV. This is the test harness for the rest of the
//! application: it is the only realistic way to verify click alignment and section
//! timing without booking a gig, and every later phase (real-time playback, cues,
//! stems) is expected to agree with it sample-for-sample given the same input.
//!
//! A silent backtrack (see [`silent_stub`]) is a legitimate input -- the expected
//! output is then click-only -- but the render pipeline itself does not special-case
//! silence anywhere. [`ramp_stub`] and [`tone_map_stub`] exist because a silent
//! backtrack can't catch a source-mapping bug: see the crate-level tests in
//! `tests/offline_render.rs`.

use crate::click::{self, ClickSynthConfig};
use crate::core::{self, CoreBus, CoreClick, CoreCue, CoreTrack, PlaybackCore, SliceSequencer};
use crate::error::RenderError;
use crate::project::{Project, Song};
use crate::sections::{self, PerformanceEntry};
use crate::smoother::Smoother;
use crate::timeline::Grid;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

pub use crate::core::db_to_linear;

/// Preloaded, mono, project-rate audio per track id. Mirrors the real loader's output
/// shape (`Arc<[f32]>` per track, per CLAUDE.md invariant 1 / §1) without doing any
/// actual file I/O or resampling -- those are later-phase concerns.
#[derive(Debug, Default, Clone)]
pub struct AudioBank(HashMap<String, Arc<[f32]>>);

impl AudioBank {
    pub fn new() -> Self {
        AudioBank(HashMap::new())
    }

    pub fn insert(&mut self, track_id: impl Into<String>, audio: Arc<[f32]>) {
        self.0.insert(track_id.into(), audio);
    }

    pub fn get(&self, track_id: &str) -> Option<&Arc<[f32]>> {
        self.0.get(track_id)
    }
}

/// Preloaded, mono, project-rate spoken-cue clips (`docs/SPEC.md` §8), keyed by
/// section index within one song. Mirrors [`AudioBank`]'s shape; populated by
/// [`crate::loader::load_cue_bank`] from the already-rendered/cached WAVs
/// [`crate::tts`] produces -- this type carries no rendering logic of its own.
#[derive(Debug, Default, Clone)]
pub struct CueBank(HashMap<usize, Arc<[f32]>>);

impl CueBank {
    pub fn new() -> Self {
        CueBank(HashMap::new())
    }

    pub fn insert(&mut self, section_index: usize, audio: Arc<[f32]>) {
        self.0.insert(section_index, audio);
    }

    pub fn get(&self, section_index: usize) -> Option<&Arc<[f32]>> {
        self.0.get(&section_index)
    }
}

/// Silence. The phase-1 default backtrack stub: with no real audio pipeline yet, a
/// click-only render is the expected, correct output (see module docs and the
/// deliverable in `docs/PROMPTS.md` phase 1).
pub fn silent_stub(frames: usize) -> Arc<[f32]> {
    vec![0.0f32; frames].into()
}

/// `f32`'s 24-bit mantissa can represent every integer up to this exactly, which is
/// what makes [`ramp_stub`] / [`decode_ramp_sample`] an exact round trip rather than a
/// lossy approximation.
pub const RAMP_STUB_MAX_FRAMES: usize = 1 << 24;

/// A stub whose sample value at frame `i` encodes `i` itself: `v[i] = i / 2^24`. A
/// zero-filled backtrack can't catch a source-frame mapping bug (wrong offset, wrong
/// entry, swapped source/performance coordinates all read back as silence); this stub
/// makes that class of bug visible by construction -- decode a sample with
/// [`decode_ramp_sample`] and compare it against the source frame index you expected
/// to be reading.
pub fn ramp_stub(frames: usize) -> Arc<[f32]> {
    assert!(
        frames <= RAMP_STUB_MAX_FRAMES,
        "ramp_stub supports at most {RAMP_STUB_MAX_FRAMES} frames (f32's exact-integer \
         range) for a lossless round trip; got {frames}"
    );
    let scale = RAMP_STUB_MAX_FRAMES as f32;
    (0..frames)
        .map(|i| i as f32 / scale)
        .collect::<Vec<f32>>()
        .into()
}

/// Inverse of [`ramp_stub`]: recover the source frame index a ramp-stub sample value
/// was generated from.
pub fn decode_ramp_sample(sample: f32) -> i64 {
    (sample * RAMP_STUB_MAX_FRAMES as f32).round() as i64
}

/// A stub with a distinct steady sine tone per `bar_frames`-sample block, cycling
/// through `freqs`. Gives decorrelated material on either side of a splice (what an
/// equal-power crossfade level check needs) and makes a reordered render audibly
/// checkable in a DAW.
pub fn tone_map_stub(
    frames: usize,
    sample_rate: u32,
    bar_frames: i64,
    freqs: &[f64],
) -> Arc<[f32]> {
    assert!(
        !freqs.is_empty(),
        "tone_map_stub needs at least one frequency"
    );
    assert!(bar_frames > 0, "tone_map_stub bar_frames must be positive");
    (0..frames)
        .map(|i| {
            let block = (i as i64 / bar_frames).rem_euclid(freqs.len() as i64) as usize;
            let freq = freqs[block];
            let t = i as f64 / sample_rate as f64;
            ((2.0 * std::f64::consts::PI * freq * t).sin() * 0.5) as f32
        })
        .collect::<Vec<f32>>()
        .into()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RenderOptions {
    /// Samples of silence prepended to the render before performance-time sample 0.
    /// Default 0 (output sample 0 = performance bar 1, beat 1). Set this to a song's
    /// `offset_samples` to have the render line up at 0:00 against the untrimmed
    /// source backtrack in a DAW.
    pub lead_in_samples: u64,
    /// Samples of silence appended after the last performance-time sample, so the
    /// final click hit's decay tail (and any crossfade reading past the nominal end)
    /// isn't truncated.
    pub tail_samples: u64,
    /// Bars of click-only count-in (§6) to render before the first entry's downbeat.
    /// Default 0 (no count-in — the render starts exactly at performance bar 0, the
    /// pre-count-in behaviour). Clamped to 0-4 per §6's stated range.
    pub count_in_bars: u32,
}

#[derive(Debug, Clone)]
pub struct RenderedAudio {
    pub sample_rate: u32,
    pub channels: u16,
    /// Interleaved samples, `channels` per frame.
    pub interleaved: Vec<f32>,
}

impl RenderedAudio {
    pub fn frame_count(&self) -> usize {
        if self.channels == 0 {
            0
        } else {
            self.interleaved.len() / self.channels as usize
        }
    }
}

/// Block size the offline renderer drives the core with. Any value would produce
/// bit-identical output (the core is block-size invariant, and
/// `tests/rt_equivalence.rs` proves it); 1024 mirrors the live path's typical device
/// buffer.
const OFFLINE_BLOCK_FRAMES: usize = 1024;

/// Render `song` (from `project`) played in `order` to an interleaved stereo buffer.
/// Bus 0 goes to the left channel, bus 1 to the right, regardless of how many buses
/// `project.bus_layout` defines or what output channels they're mapped to -- §12 asks
/// specifically for a two-channel monitoring render, not a full N-bus render.
///
/// Since phase 2 this drives the same [`PlaybackCore`] the real-time engine runs in
/// the audio callback, in [`OFFLINE_BLOCK_FRAMES`] blocks: the offline render *is*
/// the live signal path, headless.
pub fn render_song(
    project: &Project,
    song: &Song,
    order: &[PerformanceEntry],
    bank: &AudioBank,
    cues: &CueBank,
    opts: &RenderOptions,
) -> Result<RenderedAudio, RenderError> {
    let sample_rate = project.sample_rate;
    let bus_count = project.bus_layout.buses.len().max(1);

    let resolved = sections::resolve_order(song, sample_rate, order)?;
    let content_len = sections::total_length_samples(&resolved);
    let lead_in = opts.lead_in_samples as i64;
    let tail = opts.tail_samples as i64;

    let click_bus = project.click.bus;
    if click_bus >= bus_count {
        return Err(RenderError::BusIndexOutOfRange(click_bus, bus_count));
    }
    let cue_bus = project.cue.bus;
    if cue_bus >= bus_count {
        return Err(RenderError::BusIndexOutOfRange(cue_bus, bus_count));
    }

    let grid = Grid::new(sample_rate, song.bpm, song.time_signature)?;
    let count_in_bars = opts.count_in_bars.min(4);
    // Symmetric with `PlaybackCore::start`'s count-in start pulse
    // (`first.perf_start_pulse - count_in_pulses`): entries[0] always starts at
    // perf pulse 0 (resolve_order's cursor starts at 0), so this is exactly
    // `-core.perf_pos()` right after `start`.
    let count_in_samples = grid.pulse_to_sample(count_in_bars as i64 * grid.pulses_per_bar());
    let total_len = (lead_in + count_in_samples + content_len + tail).max(0) as usize;

    let mut core_tracks = Vec::with_capacity(song.tracks.len());
    for track in &song.tracks {
        if track.bus >= bus_count {
            return Err(RenderError::BusIndexOutOfRange(track.bus, bus_count));
        }
        let audio = bank
            .get(&track.id)
            .ok_or_else(|| RenderError::MissingTrack(track.id.clone()))?;
        core_tracks.push(CoreTrack {
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
    let buses: Vec<CoreBus> = if project.bus_layout.buses.is_empty() {
        vec![CoreBus {
            limiter_enabled: false,
        }]
    } else {
        project
            .bus_layout
            .buses
            .iter()
            .map(|b| CoreBus {
                limiter_enabled: b.limiter_enabled,
            })
            .collect()
    };

    let cue = CoreCue {
        bus: cue_bus,
        gain: Smoother::settled(db_to_linear(project.cue.gain_db) as f32),
    };

    let mut core = PlaybackCore::new(
        grid,
        song.offset_samples,
        core_tracks,
        click,
        cue,
        buses,
        song.sections.len(),
    );

    // Precompute the full cue schedule up front: the offline renderer, unlike the
    // live engine, always has the whole performance order in hand, so every cue gets
    // its full lead time (`docs/SPEC.md` §8) with no live-scheduling fallback needed.
    let scheduled_cues =
        crate::cue_schedule::schedule_cues(&grid, song, &resolved, |section_index| {
            cues.get(section_index).map(|clip| clip.len() as i64)
        });
    for scheduled in &scheduled_cues {
        if let Some(clip) = cues.get(scheduled.section_index) {
            core.schedule_cue(scheduled.start_sample, clip.clone());
        }
    }

    let entries: Vec<core::Entry> = resolved
        .iter()
        .map(|r| {
            let section = &song.sections[r.section_index];
            let e = core::make_entry(
                &grid,
                song.offset_samples,
                r.section_index,
                (section.start_bar - 1) as i64,
                r.perf_start_bar as i64,
                r.length_bars,
            );
            debug_assert_eq!(e.perf_start_sample, r.perf_start_sample);
            debug_assert_eq!(e.source_start_sample, r.source_start_sample);
            debug_assert_eq!(e.source_end_sample, r.source_end_sample);
            e
        })
        .collect();

    let mut bus_out: Vec<Vec<f32>> = (0..bus_count).map(|_| vec![0.0f32; total_len]).collect();

    if let Some(first) = entries.first() {
        core.start(*first, count_in_bars);
        let mut seq = SliceSequencer::new(&entries);
        // Render performance time [-count_in, content + tail): the tail keeps the
        // core running past the final entry so the last click hits' decay tails land
        // in the output instead of being truncated; the count-in extends the start
        // symmetrically backwards, `rendered` counting up from that earlier origin.
        let perf_total = count_in_samples + content_len + tail;
        let mut rendered = 0i64;
        while rendered < perf_total {
            let n = ((perf_total - rendered) as usize).min(OFFLINE_BLOCK_FRAMES);
            core.render_block(n, &mut seq);
            for (bus_idx, out) in bus_out.iter_mut().enumerate() {
                let src = core.bus_buffer(bus_idx);
                for (i, &s) in src.iter().enumerate().take(n) {
                    let out_idx = lead_in + rendered + i as i64;
                    if out_idx >= 0 && (out_idx as usize) < total_len {
                        out[out_idx as usize] = s;
                    }
                }
            }
            rendered += n as i64;
        }
    }

    let left = bus_out
        .first()
        .cloned()
        .unwrap_or_else(|| vec![0.0; total_len]);
    let right = bus_out
        .get(1)
        .cloned()
        .unwrap_or_else(|| vec![0.0; total_len]);
    let mut interleaved = Vec::with_capacity(total_len * 2);
    for i in 0..total_len {
        interleaved.push(left[i]);
        interleaved.push(right[i]);
    }

    Ok(RenderedAudio {
        sample_rate,
        channels: 2,
        interleaved,
    })
}

/// Write a rendered buffer to a 32-bit float WAV.
pub fn write_wav(path: &Path, audio: &RenderedAudio) -> Result<(), RenderError> {
    let spec = hound::WavSpec {
        channels: audio.channels,
        sample_rate: audio.sample_rate,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut writer = hound::WavWriter::create(path, spec)?;
    for &sample in &audio.interleaved {
        writer.write_sample(sample)?;
    }
    writer.finalize()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::path::RelPath;
    use crate::project::{
        AudioFileRef, Bus, BusLayout, ClickConfig, CueConfig, DownmixMode, Section, Track,
        TrackKind,
    };
    use crate::timeline::TimeSignature;

    fn test_project() -> Project {
        Project {
            schema_version: crate::project::SCHEMA_VERSION,
            name: "Test".into(),
            sample_rate: 48000,
            bus_layout: BusLayout {
                buses: vec![
                    Bus {
                        name: "Backtrack".into(),
                        output_channel: 0,
                        limiter_enabled: true,
                    },
                    Bus {
                        name: "Click/Cues".into(),
                        output_channel: 1,
                        limiter_enabled: false,
                    },
                ],
            },
            click: ClickConfig {
                bus: 1,
                gain_db: 0.0,
            },
            cue: CueConfig::default(),
            songs: vec![],
        }
    }

    fn test_song() -> Song {
        Song {
            id: "song1".into(),
            title: "Test Song".into(),
            bpm: 178.0,
            time_signature: TimeSignature::FOUR_FOUR,
            offset_samples: 0,
            count_in_bars: 1,
            accent_pattern: vec![],
            sections: vec![Section {
                name: "Full".into(),
                start_bar: 1,
                length_bars: 4,
                loopable: false,
                cue_text: None,
                cue_lead_beats: 4,
            }],
            tracks: vec![Track {
                id: "bt".into(),
                name: "Backtrack".into(),
                file: AudioFileRef {
                    path: RelPath::new("audio/bt.wav").unwrap(),
                    sha256: None,
                    frames: None,
                },
                gain_db: 0.0,
                muted: false,
                bus: 0,
                downmix: DownmixMode::Sum,
                kind: TrackKind::Backtrack,
            }],
            disabled: false,
        }
    }

    #[test]
    fn click_only_render_leaves_left_channel_silent() {
        let project = test_project();
        let song = test_song();
        let mut bank = AudioBank::new();
        bank.insert("bt", silent_stub(500_000));
        let order = [PerformanceEntry::once(0)];
        let audio = render_song(
            &project,
            &song,
            &order,
            &bank,
            &CueBank::new(),
            &RenderOptions::default(),
        )
        .unwrap();

        for frame in 0..audio.frame_count() {
            let left = audio.interleaved[frame * 2];
            assert_eq!(left, 0.0, "left channel should be silent at frame {frame}");
        }
        // Right channel should have at least one nonzero (click) sample.
        assert!((0..audio.frame_count()).any(|f| audio.interleaved[f * 2 + 1] != 0.0));
    }

    #[test]
    fn missing_track_in_bank_is_an_error() {
        let project = test_project();
        let song = test_song();
        let bank = AudioBank::new(); // no "bt" inserted
        let order = [PerformanceEntry::once(0)];
        let err = render_song(
            &project,
            &song,
            &order,
            &bank,
            &CueBank::new(),
            &RenderOptions::default(),
        );
        assert!(matches!(err, Err(RenderError::MissingTrack(_))));
    }

    #[test]
    fn lead_in_shifts_output_start() {
        let project = test_project();
        let song = test_song();
        let mut bank = AudioBank::new();
        bank.insert("bt", silent_stub(500_000));
        let order = [PerformanceEntry::once(0)];
        let opts = RenderOptions {
            lead_in_samples: 1000,
            tail_samples: 0,
            count_in_bars: 0,
        };
        let audio = render_song(&project, &song, &order, &bank, &CueBank::new(), &opts).unwrap();
        // No click transient before sample 1000 (bar 1 beat 1 accent lands at 1000).
        for frame in 0..1000 {
            assert_eq!(audio.interleaved[frame * 2 + 1], 0.0);
        }
        assert_ne!(audio.interleaved[1000 * 2 + 1], 0.0);
    }

    #[test]
    fn ramp_stub_round_trips_through_decode() {
        let stub = ramp_stub(1000);
        for i in [0usize, 1, 500, 999] {
            assert_eq!(decode_ramp_sample(stub[i]), i as i64);
        }
    }
}
