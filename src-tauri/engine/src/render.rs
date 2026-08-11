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
use crate::crossfade;
use crate::error::RenderError;
use crate::project::{Project, Song, Track};
use crate::sections::{self, PerformanceEntry, ResolvedEntry};
use crate::timeline::Grid;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

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

/// Render `song` (from `project`) played in `order` to an interleaved stereo buffer.
/// Bus 0 goes to the left channel, bus 1 to the right, regardless of how many buses
/// `project.bus_layout` defines or what output channels they're mapped to -- §12 asks
/// specifically for a two-channel monitoring render, not a full N-bus render.
pub fn render_song(
    project: &Project,
    song: &Song,
    order: &[PerformanceEntry],
    bank: &AudioBank,
    opts: &RenderOptions,
) -> Result<RenderedAudio, RenderError> {
    let sample_rate = project.sample_rate;
    let bus_count = project.bus_layout.buses.len().max(1);

    let resolved = sections::resolve_order(song, sample_rate, order)?;
    let content_len = sections::total_length_samples(&resolved);
    let lead_in = opts.lead_in_samples as i64;
    let tail = opts.tail_samples as i64;
    let total_len = (lead_in + content_len + tail).max(0) as usize;

    let mut bus_buffers: Vec<Vec<f32>> = (0..bus_count).map(|_| vec![0.0f32; total_len]).collect();

    copy_track_audio(&mut bus_buffers, song, bank, &resolved, lead_in, bus_count)?;

    for pair in resolved.windows(2) {
        let (prev, next) = (&pair[0], &pair[1]);
        if prev.source_end_sample == next.source_start_sample {
            continue; // contiguous: a straight continuation, not a splice -- no fade.
        }
        apply_crossfade(
            &mut bus_buffers,
            &song.tracks,
            bank,
            prev,
            next,
            sample_rate,
            lead_in,
        )?;
    }

    render_click_into_buses(
        &mut bus_buffers,
        project,
        song,
        &resolved,
        sample_rate,
        lead_in,
        bus_count,
    )?;

    let left = bus_buffers
        .first()
        .cloned()
        .unwrap_or_else(|| vec![0.0; total_len]);
    let right = bus_buffers
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

/// Sum every track's plain (un-crossfaded) audio into its bus across every resolved
/// entry. Multiple tracks assigned to the same bus are additive (`+=`), per §4 "sum
/// sources per bus." The crossfade pass below overwrites the boundary samples this
/// step wrote wherever a splice needs blending.
fn copy_track_audio(
    bus_buffers: &mut [Vec<f32>],
    song: &Song,
    bank: &AudioBank,
    resolved: &[ResolvedEntry],
    lead_in: i64,
    bus_count: usize,
) -> Result<(), RenderError> {
    for track in &song.tracks {
        if track.muted {
            continue;
        }
        if track.bus >= bus_count {
            return Err(RenderError::BusIndexOutOfRange(track.bus, bus_count));
        }
        let audio = bank
            .get(&track.id)
            .ok_or_else(|| RenderError::MissingTrack(track.id.clone()))?;
        let gain = db_to_linear(track.gain_db) as f32;
        let buf = &mut bus_buffers[track.bus];

        for entry in resolved {
            let len = entry.perf_length_samples();
            for i in 0..len {
                let out_idx = entry.perf_start_sample + i + lead_in;
                if out_idx < 0 {
                    continue;
                }
                let out_idx = out_idx as usize;
                if out_idx >= buf.len() {
                    break;
                }
                let src_frame = entry.source_start_sample + i;
                buf[out_idx] += read_track_sample(audio, src_frame) * gain;
            }
        }
    }
    Ok(())
}

/// Apply the 15 ms equal-power crossfade (§7) at one non-contiguous splice: for the
/// fade window at the start of `next`, overwrite each affected bus with the blend of
/// the outgoing track(s) (read past `prev`'s nominal end, "kept alive") and the
/// incoming track(s) (read from `next`'s start). Buses fed by more than one track are
/// summed *before* the fade weighting is applied per side, which is equivalent to
/// applying the fade per track and summing after (the fade is a linear operation).
fn apply_crossfade(
    bus_buffers: &mut [Vec<f32>],
    tracks: &[Track],
    bank: &AudioBank,
    prev: &ResolvedEntry,
    next: &ResolvedEntry,
    sample_rate: u32,
    lead_in: i64,
) -> Result<(), RenderError> {
    let fade_len =
        crossfade::crossfade_length_samples(sample_rate, Some(next.perf_length_samples()));
    if fade_len <= 0 {
        return Ok(());
    }

    for (bus_idx, bus_buf) in bus_buffers.iter_mut().enumerate() {
        let mut sources: Vec<(&Arc<[f32]>, f32)> = Vec::new();
        for t in tracks.iter().filter(|t| t.bus == bus_idx && !t.muted) {
            let audio = bank
                .get(&t.id)
                .ok_or_else(|| RenderError::MissingTrack(t.id.clone()))?;
            sources.push((audio, db_to_linear(t.gain_db) as f32));
        }
        if sources.is_empty() {
            continue; // nothing on this bus reads from a track; the click bus lands here.
        }

        for i in 0..fade_len {
            let (gain_out, gain_in) = crossfade::equal_power_gains(i as f64 / fade_len as f64);
            let mut value = 0.0f32;
            for (audio, gain) in &sources {
                let out_frame = prev.source_end_sample + i;
                let in_frame = next.source_start_sample + i;
                value += gain_out * gain * read_track_sample(audio, out_frame);
                value += gain_in * gain * read_track_sample(audio, in_frame);
            }
            let out_idx = next.perf_start_sample + i + lead_in;
            if out_idx < 0 {
                continue;
            }
            let out_idx = out_idx as usize;
            if out_idx < bus_buf.len() {
                bus_buf[out_idx] = value;
            }
        }
    }
    Ok(())
}

/// Generate the click across the whole (gapless, by construction) performance-time
/// span covered by `resolved` and mix it additively into the configured click bus.
fn render_click_into_buses(
    bus_buffers: &mut [Vec<f32>],
    project: &Project,
    song: &Song,
    resolved: &[ResolvedEntry],
    sample_rate: u32,
    lead_in: i64,
    bus_count: usize,
) -> Result<(), RenderError> {
    let (first, last) = match (resolved.first(), resolved.last()) {
        (Some(f), Some(l)) => (f, l),
        _ => return Ok(()), // empty programme: nothing to click.
    };
    let click_bus = project.click.bus;
    if click_bus >= bus_count {
        return Err(RenderError::BusIndexOutOfRange(click_bus, bus_count));
    }
    let grid = Grid::new(sample_rate, song.bpm, song.time_signature)?;
    let click_gain = db_to_linear(project.click.gain_db) as f32;

    let total_len = bus_buffers[click_bus].len();
    let mut click_buf = vec![0.0f32; total_len];
    click::render_click_pulses(
        &grid,
        &song.accent_pattern,
        first.perf_start_pulse,
        last.perf_end_pulse,
        &ClickSynthConfig::default(),
        lead_in,
        &mut click_buf,
    );

    for (dst, src) in bus_buffers[click_bus].iter_mut().zip(click_buf.iter()) {
        *dst += *src * click_gain;
    }
    Ok(())
}

fn db_to_linear(db: f64) -> f64 {
    10f64.powf(db / 20.0)
}

fn read_track_sample(audio: &Arc<[f32]>, frame: i64) -> f32 {
    if frame < 0 {
        return 0.0;
    }
    audio.get(frame as usize).copied().unwrap_or(0.0)
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
        AudioFileRef, Bus, BusLayout, ClickConfig, DownmixMode, Section, TrackKind,
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
        let audio = render_song(&project, &song, &order, &bank, &RenderOptions::default()).unwrap();

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
        let err = render_song(&project, &song, &order, &bank, &RenderOptions::default());
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
        };
        let audio = render_song(&project, &song, &order, &bank, &opts).unwrap();
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
