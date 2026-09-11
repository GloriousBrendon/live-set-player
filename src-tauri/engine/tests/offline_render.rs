//! Integration tests for the offline renderer (`docs/SPEC.md` §12), driving the crate
//! exactly the way `examples/render_click.rs` and a future real-time engine would:
//! through the public API only.
//!
//! Three things are checked here that no unit test inside `render.rs` can check on its
//! own, because they only exist once the whole pipeline runs end to end:
//!
//! 1. **Click timing** -- every transient lands on the exact sample the timeline grid
//!    predicts, in 4/4 and 7/8, including over a full 10-minute render.
//! 2. **Source mapping** -- using [`lsp_engine::render::ramp_stub`], because a silent
//!    backtrack reads back as zero whether or not the source-frame math is right.
//!    This is the test that would catch an off-by-`offset_samples` bug, a swapped
//!    section, or a source/performance coordinate mix-up.
//! 3. **The crossfade** -- applied at every non-contiguous splice, absent at every
//!    contiguous one.

use lsp_engine::crossfade;
use lsp_engine::path::RelPath;
use lsp_engine::project::{
    AudioFileRef, Bus, BusLayout, ClickConfig, CueConfig, DownmixMode, Project, Section, Song,
    Track, TrackKind,
};
use lsp_engine::render::{self, AudioBank, CueBank, RenderOptions, RenderedAudio};
use lsp_engine::sections::PerformanceEntry;
use lsp_engine::timeline::{Grid, TimeSignature};

// ---------------------------------------------------------------------------------
// Shared test fixtures
// ---------------------------------------------------------------------------------

fn project(sample_rate: u32) -> Project {
    Project {
        schema_version: lsp_engine::project::SCHEMA_VERSION,
        name: "Test".into(),
        sample_rate,
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
        gap_seconds: 0.0,
        songs: vec![],
    }
}

fn backtrack_track() -> Track {
    Track {
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
    }
}

fn single_section_song(
    bpm: f64,
    ts: TimeSignature,
    length_bars: u32,
    accent_pattern: Vec<u8>,
) -> Song {
    Song {
        id: "song1".into(),
        title: "Test Song".into(),
        bpm,
        time_signature: ts,
        offset_samples: 0,
        count_in_bars: 1,
        accent_pattern,
        sections: vec![Section {
            name: "Full".into(),
            start_bar: 1,
            length_bars,
            loopable: false,
            cue_text: None,
            cue_lead_beats: 4,
        }],
        tracks: vec![backtrack_track()],
        auto_continue: false,
        disabled: false,
    }
}

/// The four-section song used by both `sections.rs`'s unit tests and here: Intro (1,4)
/// Verse (5,8) Chorus (13,8) Bridge (21,4), contiguous in source order.
fn four_section_song(bpm: f64, ts: TimeSignature) -> Song {
    Song {
        id: "song1".into(),
        title: "Test Song".into(),
        bpm,
        time_signature: ts,
        offset_samples: 0,
        count_in_bars: 1,
        accent_pattern: vec![],
        sections: vec![
            Section {
                name: "Intro".into(),
                start_bar: 1,
                length_bars: 4,
                loopable: false,
                cue_text: None,
                cue_lead_beats: 4,
            },
            Section {
                name: "Verse".into(),
                start_bar: 5,
                length_bars: 8,
                loopable: false,
                cue_text: None,
                cue_lead_beats: 4,
            },
            Section {
                name: "Chorus".into(),
                start_bar: 13,
                length_bars: 8,
                loopable: true,
                cue_text: None,
                cue_lead_beats: 4,
            },
            Section {
                name: "Bridge".into(),
                start_bar: 21,
                length_bars: 4,
                loopable: false,
                cue_text: None,
                cue_lead_beats: 4,
            },
        ],
        tracks: vec![backtrack_track()],
        auto_continue: false,
        disabled: false,
    }
}

fn left(audio: &RenderedAudio, frame: usize) -> f32 {
    audio.interleaved[frame * 2]
}

fn right(audio: &RenderedAudio, frame: usize) -> f32 {
    audio.interleaved[frame * 2 + 1]
}

/// Index of the largest-magnitude sample of the right (click) channel within
/// `[center - radius, center + radius]`. Used to confirm a transient's peak sits
/// exactly where expected, not just "somewhere nearby."
fn peak_index_in_window(audio: &RenderedAudio, center: usize, radius: usize) -> usize {
    let lo = center.saturating_sub(radius);
    let hi = (center + radius).min(audio.frame_count() - 1);
    (lo..=hi)
        .max_by(|&a, &b| {
            right(audio, a)
                .abs()
                .partial_cmp(&right(audio, b).abs())
                .unwrap()
        })
        .unwrap()
}

// ---------------------------------------------------------------------------------
// 1. Click timing
// ---------------------------------------------------------------------------------

#[test]
fn click_transients_land_on_exact_grid_samples_4_4() {
    let sample_rate = 48000u32;
    let bpm = 178.0;
    let ts = TimeSignature::FOUR_FOUR;
    let proj = project(sample_rate);
    let song = single_section_song(bpm, ts, 8, vec![]); // 8 bars = 32 pulses
    let mut bank = AudioBank::new();
    bank.insert("bt", render::silent_stub(2_000_000));
    let order = [PerformanceEntry::once(0)];
    let audio = render::render_song(
        &proj,
        &song,
        &order,
        &bank,
        &CueBank::new(),
        &RenderOptions::default(),
    )
    .unwrap();

    let grid = Grid::new(sample_rate, bpm, ts).unwrap();
    for pulse in 0..32i64 {
        let expected = grid.pulse_to_sample(pulse) as usize;
        assert_eq!(
            peak_index_in_window(&audio, expected, 50),
            expected,
            "pulse {pulse} transient peak"
        );
        assert_eq!(left(&audio, expected), 0.0, "left channel must stay silent");
    }
}

#[test]
fn click_transients_and_accents_land_correctly_in_7_8() {
    let sample_rate = 48000u32;
    let bpm = 178.0;
    let ts = TimeSignature {
        numerator: 7,
        denominator: 8,
    };
    let accent_pattern = vec![2, 0, 0, 1, 0, 0, 0];
    let proj = project(sample_rate);
    let song = single_section_song(bpm, ts, 4, accent_pattern.clone()); // 4 bars = 28 pulses
    let mut bank = AudioBank::new();
    bank.insert("bt", render::silent_stub(2_000_000));
    let order = [PerformanceEntry::once(0)];
    let audio = render::render_song(
        &proj,
        &song,
        &order,
        &bank,
        &CueBank::new(),
        &RenderOptions::default(),
    )
    .unwrap();

    let grid = Grid::new(sample_rate, bpm, ts).unwrap();
    let default_cfg = lsp_engine::click::ClickSynthConfig::default();
    for pulse in 0..28i64 {
        let expected = grid.pulse_to_sample(pulse) as usize;
        assert_eq!(
            peak_index_in_window(&audio, expected, 50),
            expected,
            "pulse {pulse} transient peak"
        );
        let idx_in_bar = (pulse.rem_euclid(7)) as usize;
        let expected_gain = if accent_pattern[idx_in_bar] > 0 {
            default_cfg.accent_gain
        } else {
            default_cfg.base_gain
        };
        assert!(
            (right(&audio, expected).abs() - expected_gain).abs() < 1e-5,
            "pulse {pulse} (bar-position {idx_in_bar}): expected peak magnitude {expected_gain}, got {}",
            right(&audio, expected).abs()
        );
    }
}

#[test]
fn ten_minute_render_final_transient_is_exact() {
    let sample_rate = 48000u32;
    let bpm = 178.0;
    let ts = TimeSignature::FOUR_FOUR;
    let grid = Grid::new(sample_rate, bpm, ts).unwrap();

    // 10 minutes of pulses, rounded up to whole bars.
    let total_pulses = (bpm * 10.0).ceil() as i64; // 1780
    let length_bars = ((total_pulses as f64) / 4.0).ceil() as u32; // 445
    let last_pulse = length_bars as i64 * 4 - 1;

    let proj = project(sample_rate);
    let song = single_section_song(bpm, ts, length_bars, vec![]);
    let mut bank = AudioBank::new();
    // Backtrack length must cover the whole render; silent stub sized generously.
    bank.insert(
        "bt",
        render::silent_stub(
            grid.pulse_to_sample(length_bars as i64 * 4) as usize + sample_rate as usize,
        ),
    );
    let order = [PerformanceEntry::once(0)];
    let opts = RenderOptions {
        lead_in_samples: 0,
        tail_samples: sample_rate as u64, // headroom for the last click's decay tail
        count_in_bars: 0,
    };
    let audio = render::render_song(&proj, &song, &order, &bank, &CueBank::new(), &opts).unwrap();

    let expected = grid.pulse_to_sample(last_pulse) as usize;
    assert_eq!(
        peak_index_in_window(&audio, expected, 50),
        expected,
        "final transient (pulse {last_pulse}) after a 10 minute render"
    );

    // This is the same guarantee `timeline.rs`'s drift tests prove numerically;
    // restated here as an end-to-end assertion that the render pipeline (buffer
    // indexing, lead-in addition) didn't reintroduce an off-by-something. Naive
    // truncating accumulation would land at `last_pulse * (spp as i64)` (a fixed
    // per-step truncation error times the number of steps); confirm the real
    // pipeline's answer has drifted well clear of that wrong value.
    let spp = grid.samples_per_pulse();
    let naive_position = last_pulse * (spp as i64);
    let naive_error_samples = (expected as i64 - naive_position).abs();
    assert!(
        naive_error_samples > 100,
        "sanity: naive accumulation should have drifted noticeably by 10 minutes in, got {naive_error_samples} samples"
    );
}

#[test]
fn click_only_render_writes_a_valid_wav_readable_by_hound() {
    let sample_rate = 44100u32;
    let bpm = 91.0;
    let ts = TimeSignature::FOUR_FOUR;
    let proj = project(sample_rate);
    let song = single_section_song(bpm, ts, 4, vec![]);
    let mut bank = AudioBank::new();
    bank.insert("bt", render::silent_stub(500_000));
    let order = [PerformanceEntry::once(0)];
    let audio = render::render_song(
        &proj,
        &song,
        &order,
        &bank,
        &CueBank::new(),
        &RenderOptions::default(),
    )
    .unwrap();

    let path = std::env::temp_dir().join(format!("lsp_engine_test_{}.wav", std::process::id()));
    render::write_wav(&path, &audio).unwrap();

    let reader = hound::WavReader::open(&path).unwrap();
    let spec = reader.spec();
    assert_eq!(spec.channels, 2);
    assert_eq!(spec.sample_rate, sample_rate);
    assert_eq!(spec.bits_per_sample, 32);
    assert_eq!(spec.sample_format, hound::SampleFormat::Float);
    assert_eq!(reader.len() as usize, audio.interleaved.len());

    let _ = std::fs::remove_file(&path);
}

// ---------------------------------------------------------------------------------
// 2. Source mapping (the test a silent stub can't do)
// ---------------------------------------------------------------------------------

#[test]
fn out_of_order_programme_reads_from_correct_source_frames() {
    let sample_rate = 48000u32;
    let bpm = 178.0;
    let ts = TimeSignature::FOUR_FOUR;
    let grid = Grid::new(sample_rate, bpm, ts).unwrap();

    let proj = project(sample_rate);
    let song = four_section_song(bpm, ts);

    // Backtrack must cover source bars up to 25 (Bridge ends at bar 24) plus headroom.
    let backtrack_frames = grid.bar_beat_to_sample(30, 0) as usize;
    let mut bank = AudioBank::new();
    bank.insert("bt", render::ramp_stub(backtrack_frames));

    // section indices: 0=Intro 1=Verse 2=Chorus 3=Bridge -- deliberately out of source
    // order, and every boundary here is a splice (no two sections are source-adjacent
    // in this order).
    let order = [
        PerformanceEntry::once(2), // Chorus
        PerformanceEntry::once(1), // Verse
        PerformanceEntry::once(2), // Chorus
        PerformanceEntry::once(3), // Bridge
        PerformanceEntry::once(0), // Intro
    ];

    let resolved = lsp_engine::sections::resolve_order(&song, sample_rate, &order).unwrap();
    let audio = render::render_song(
        &proj,
        &song,
        &order,
        &bank,
        &CueBank::new(),
        &RenderOptions::default(),
    )
    .unwrap();

    for (i, entry) in resolved.iter().enumerate() {
        let fade_len =
            crossfade::crossfade_length_samples(sample_rate, Some(entry.perf_length_samples()));
        // Entry 0 gets no incoming fade (nothing precedes it); every later entry here
        // follows a splice, so its first `fade_len` samples are blended and skipped.
        let skip_start = if i == 0 { 0 } else { fade_len };
        let len = entry.perf_length_samples();
        assert!(
            len > skip_start + 100,
            "entry {i} too short for this test's sampling"
        );

        let offsets = [skip_start, skip_start + 50, len / 2, len - 1];
        for &offset in &offsets {
            let perf_idx = (entry.perf_start_sample + offset) as usize;
            let sample = left(&audio, perf_idx);
            let decoded = render::decode_ramp_sample(sample);
            let expected_source_frame = entry.source_start_sample + offset;
            assert_eq!(
                decoded, expected_source_frame,
                "entry {i} (section {}), offset {offset}: decoded frame {decoded}, expected {expected_source_frame}",
                entry.section_index
            );
        }
    }
}

// ---------------------------------------------------------------------------------
// 3. Crossfade: present at splices, absent at contiguous joins
// ---------------------------------------------------------------------------------

#[test]
fn crossfade_holds_constant_average_power_across_a_splice() {
    let sample_rate = 48000u32;
    let bpm = 178.0;
    let ts = TimeSignature::FOUR_FOUR;
    let grid = Grid::new(sample_rate, bpm, ts).unwrap();

    let proj = project(sample_rate);
    let song = four_section_song(bpm, ts);

    let backtrack_frames = grid.bar_beat_to_sample(30, 0) as usize;
    let bar_frames = (grid.samples_per_pulse() * grid.pulses_per_bar() as f64).round() as i64;
    // One well-separated, roughly incommensurate frequency per source bar-block,
    // covering bars 0..24 (0-based) plus headroom: Intro=bars0-3, Verse=bars4-11,
    // Chorus=bars12-19, Bridge=bars20-23.
    let mut freqs = vec![0.0f64; 30];
    for (i, f) in freqs.iter_mut().enumerate() {
        *f = match i {
            0..=3 => 300.0,
            4..=11 => 500.0,
            12..=19 => 800.0,
            _ => 1200.0,
        };
    }
    let mut bank = AudioBank::new();
    bank.insert(
        "bt",
        render::tone_map_stub(backtrack_frames, sample_rate, bar_frames, &freqs),
    );

    let order = [
        PerformanceEntry::once(2), // Chorus (800Hz)
        PerformanceEntry::once(1), // Verse (500Hz)
        PerformanceEntry::once(3), // Bridge (1200Hz)
        PerformanceEntry::once(0), // Intro (300Hz)
    ];
    let resolved = lsp_engine::sections::resolve_order(&song, sample_rate, &order).unwrap();
    let audio = render::render_song(
        &proj,
        &song,
        &order,
        &bank,
        &CueBank::new(),
        &RenderOptions::default(),
    )
    .unwrap();

    // Stub amplitude is a fixed 0.5, so any two segments have equal steady-state
    // power; an equal-power crossfade between decorrelated tones should hold that
    // power roughly constant through the splice. Expected RMS of a 0.5-amplitude
    // sine averaged over enough cycles is 0.5/sqrt(2).
    let expected_rms = 0.5 / (2.0f64).sqrt();

    for (i, entry) in resolved.iter().enumerate().skip(1) {
        let fade_len =
            crossfade::crossfade_length_samples(sample_rate, Some(entry.perf_length_samples()))
                as usize;
        let start = entry.perf_start_sample as usize;
        let sum_sq: f64 = (0..fade_len)
            .map(|i| {
                let v = left(&audio, start + i) as f64;
                v * v
            })
            .sum();
        let rms = (sum_sq / fade_len as f64).sqrt();
        let rel_error = (rms - expected_rms).abs() / expected_rms;
        assert!(
            rel_error < 0.2,
            "entry {i}: crossfade RMS {rms:.4} deviates {:.1}% from expected {expected_rms:.4}",
            rel_error * 100.0
        );
    }
}

#[test]
fn contiguous_boundaries_get_no_crossfade() {
    let sample_rate = 48000u32;
    let bpm = 178.0;
    let ts = TimeSignature::FOUR_FOUR;
    let grid = Grid::new(sample_rate, bpm, ts).unwrap();

    let proj = project(sample_rate);
    let song = four_section_song(bpm, ts);

    let backtrack_frames = grid.bar_beat_to_sample(30, 0) as usize;
    let mut bank = AudioBank::new();
    bank.insert("bt", render::ramp_stub(backtrack_frames));

    // Natural forward order: Intro, Verse, Chorus, Bridge. In this song these are
    // source-contiguous (Intro ends at source bar 4, Verse starts at bar 5, etc.), so
    // every boundary should be a straight continuation with *no* fade applied.
    let order = [
        PerformanceEntry::once(0),
        PerformanceEntry::once(1),
        PerformanceEntry::once(2),
        PerformanceEntry::once(3),
    ];
    let resolved = lsp_engine::sections::resolve_order(&song, sample_rate, &order).unwrap();
    let audio = render::render_song(
        &proj,
        &song,
        &order,
        &bank,
        &CueBank::new(),
        &RenderOptions::default(),
    )
    .unwrap();

    // Sanity: this song really is source-contiguous end to end, so a plain ramp read
    // across the whole programme should decode as one unbroken sequence.
    for w in resolved.windows(2) {
        assert_eq!(
            w[0].source_end_sample, w[1].source_start_sample,
            "test fixture assumption broken: these sections are not source-contiguous"
        );
    }

    for entry in &resolved {
        let radius = 200usize.min((entry.perf_length_samples() / 2) as usize);
        let start = entry.perf_start_sample as usize;
        let end = entry.perf_end_sample as usize;
        for offset in [0usize, radius] {
            for &idx in &[start + offset, end.saturating_sub(offset + 1)] {
                let sample = left(&audio, idx);
                let decoded = render::decode_ramp_sample(sample);
                let expected = entry.source_start_sample + (idx as i64 - entry.perf_start_sample);
                assert_eq!(
                    decoded, expected,
                    "entry (section {}) at perf sample {idx}: decoded {decoded}, expected {expected} \
                     -- a spurious crossfade at a contiguous boundary would corrupt this",
                    entry.section_index
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------------
// 4. Count-in (§6)
// ---------------------------------------------------------------------------------

/// Count-in in 7/8 with a real accent pattern: every click before the downbeat lands
/// on the exact negative-pulse sample the grid predicts, respects the same
/// bar-relative accent pattern the rest of the song uses (a count-in bar accents its
/// own beat 1 the same way any other bar would), the backtrack stays silent for the
/// *entire* count-in region -- checked with `ramp_stub`, not a silent stub, and with
/// the section's source start pushed well past the count-in's length in samples, so a
/// leaked track read during count-in would decode to a wrong nonzero frame instead of
/// coincidentally matching silence -- and the backtrack picks up at exactly the right
/// source frame the instant the downbeat lands.
#[test]
fn count_in_is_click_only_and_respects_the_accent_pattern_in_7_8() {
    let sample_rate = 48000u32;
    let bpm = 178.0;
    let ts = TimeSignature {
        numerator: 7,
        denominator: 8,
    };
    let accent_pattern = vec![2, 0, 0, 1, 0, 0, 0];
    let proj = project(sample_rate);
    let mut song = single_section_song(bpm, ts, 4, accent_pattern.clone());
    // Push the section's source start well past the count-in's length in samples, so
    // an accidental track read during count-in (the bug this test guards against)
    // would decode to a wrong nonzero source frame rather than silently reading as
    // "before frame 0" silence.
    song.sections[0].start_bar = 20;
    let count_in_bars = 1u32;

    let grid = Grid::new(sample_rate, bpm, ts).unwrap();
    let mut bank = AudioBank::new();
    bank.insert(
        "bt",
        render::ramp_stub(grid.bar_beat_to_sample(40, 0) as usize),
    );
    let order = [PerformanceEntry::once(0)];
    let opts = RenderOptions {
        lead_in_samples: 0,
        tail_samples: 0,
        count_in_bars,
    };
    let audio = render::render_song(&proj, &song, &order, &bank, &CueBank::new(), &opts).unwrap();

    let ppb = grid.pulses_per_bar(); // 7
    let count_in_pulses = count_in_bars as i64 * ppb;
    let count_in_samples = grid.pulse_to_sample(count_in_pulses);
    let default_cfg = lsp_engine::click::ClickSynthConfig::default();

    // Every count-in click lands exactly where the grid predicts and carries the
    // correct accent for its position within the bar.
    for pulse in -count_in_pulses..0 {
        let out_idx = (grid.pulse_to_sample(pulse) + count_in_samples) as usize;
        assert_eq!(
            peak_index_in_window(&audio, out_idx, 50),
            out_idx,
            "count-in pulse {pulse} transient"
        );
        let idx_in_bar = pulse.rem_euclid(ppb) as usize;
        let expected_gain = if accent_pattern[idx_in_bar] > 0 {
            default_cfg.accent_gain
        } else {
            default_cfg.base_gain
        };
        assert!(
            (right(&audio, out_idx).abs() - expected_gain).abs() < 1e-5,
            "count-in pulse {pulse} (bar-position {idx_in_bar}): expected peak magnitude \
             {expected_gain}, got {}",
            right(&audio, out_idx).abs()
        );
    }

    // Backtrack silent through the entire count-in region, not just at click samples.
    for frame in 0..count_in_samples as usize {
        assert_eq!(
            left(&audio, frame),
            0.0,
            "backtrack must stay silent during count-in (frame {frame})"
        );
    }

    // The downbeat lands exactly at count_in_samples, is accented (beat 1), and the
    // backtrack becomes live there, reading from the section's true source start.
    let downbeat_idx = count_in_samples as usize;
    assert_eq!(peak_index_in_window(&audio, downbeat_idx, 50), downbeat_idx);
    assert!(
        (right(&audio, downbeat_idx).abs() - default_cfg.accent_gain).abs() < 1e-5,
        "downbeat must be accented"
    );
    let expected_source_start =
        grid.pulse_to_sample(grid.bar_to_pulse((song.sections[0].start_bar - 1) as i64));
    assert_eq!(
        render::decode_ramp_sample(left(&audio, downbeat_idx)),
        expected_source_start,
        "backtrack must pick up at the section's true source start exactly at the downbeat"
    );
}

// ---------------------------------------------------------------------------------
// 4. Spoken cues (§8): end-anchored scheduling, end to end
// ---------------------------------------------------------------------------------

/// A short cue (Verse) and a long cue (Bridge), rendered through the full pipeline
/// (not just `cue_schedule`'s unit tests), both finish speaking exactly
/// `cue_lead_beats` pulses before their section's downbeat -- the "done when"
/// criterion from the original ask, verified against the actual rendered right
/// channel rather than re-deriving the formula.
#[test]
fn short_and_long_cues_finish_the_same_distance_before_their_downbeats_in_a_real_render() {
    let sample_rate = 48000u32;
    let bpm = 178.0;
    let ts = TimeSignature::FOUR_FOUR;
    let proj = project(sample_rate);
    let grid = Grid::new(sample_rate, bpm, ts).unwrap();

    let song = Song {
        id: "song1".into(),
        title: "Cue Test Song".into(),
        bpm,
        time_signature: ts,
        offset_samples: 0,
        count_in_bars: 0,
        accent_pattern: vec![],
        sections: vec![
            Section {
                name: "Intro".into(),
                start_bar: 1,
                length_bars: 4,
                loopable: false,
                cue_text: None, // no cue
                cue_lead_beats: 4,
            },
            Section {
                name: "Verse".into(),
                start_bar: 5,
                length_bars: 8,
                loopable: false,
                cue_text: Some("Verse".into()),
                cue_lead_beats: 4,
            },
            Section {
                name: "Bridge".into(),
                start_bar: 13,
                length_bars: 8,
                loopable: false,
                cue_text: Some("last time through the bridge".into()),
                cue_lead_beats: 4,
            },
        ],
        tracks: vec![backtrack_track()],
        auto_continue: false,
        disabled: false,
    };

    let mut bank = AudioBank::new();
    bank.insert("bt", render::silent_stub(2_000_000));

    // Distinct constant-amplitude "clips" so their presence in the right channel is
    // unambiguous against both silence and click transients. Short: 0.3s. Long: 1.5s.
    const CUE_AMPLITUDE: f32 = 0.42;
    let short_clip: std::sync::Arc<[f32]> =
        vec![CUE_AMPLITUDE; (0.3 * sample_rate as f64) as usize].into();
    let long_clip: std::sync::Arc<[f32]> =
        vec![CUE_AMPLITUDE; (1.5 * sample_rate as f64) as usize].into();
    let mut cues = CueBank::new();
    cues.insert(1, short_clip.clone()); // Verse
    cues.insert(2, long_clip.clone()); // Bridge

    let order = [
        PerformanceEntry::once(0),
        PerformanceEntry::once(1),
        PerformanceEntry::once(2),
    ];
    let audio = render::render_song(
        &proj,
        &song,
        &order,
        &bank,
        &cues,
        &RenderOptions::default(),
    )
    .unwrap();

    // Independently resolve where each section's downbeat lands, exactly the way
    // `sections::resolve_order` (not `cue_schedule`) computes it, so this test doesn't
    // just re-check `cue_schedule`'s own formula against itself.
    let resolved = lsp_engine::sections::resolve_order(&song, sample_rate, &order).unwrap();
    let verse_downbeat_pulse = resolved[1].perf_start_pulse;
    let bridge_downbeat_pulse = resolved[2].perf_start_pulse;

    for (clip, downbeat_pulse, cue_lead_beats, label) in [
        (&short_clip, verse_downbeat_pulse, 4i64, "Verse (short cue)"),
        (&long_clip, bridge_downbeat_pulse, 4i64, "Bridge (long cue)"),
    ] {
        let expected_end_sample = grid.pulse_to_sample(downbeat_pulse - cue_lead_beats) as usize;
        let expected_start_sample = expected_end_sample - clip.len();

        // The clip's last sample lands exactly one sample before the downbeat-minus-
        // lead-beats point, at the expected amplitude.
        assert!(
            (right(&audio, expected_end_sample - 1) - CUE_AMPLITUDE).abs() < 1e-6,
            "{label}: expected the cue's last sample at {}, got {}",
            expected_end_sample - 1,
            right(&audio, expected_end_sample - 1)
        );
        // The clip's midpoint -- far enough from either edge to be clear of a
        // neighbouring click hit's decay tail bleeding onto the same bus, unlike a
        // boundary sample would be -- confirms the clip is present at the expected
        // absolute position, not just its very last sample.
        let mid = expected_start_sample + clip.len() / 2;
        assert!(
            (right(&audio, mid) - CUE_AMPLITUDE).abs() < 1e-6,
            "{label}: expected the cue's midpoint at {mid}, got {}",
            right(&audio, mid)
        );
        // Nothing from this clip bleeds past its end (no click hit happens to land
        // exactly at this amplitude, so an exact non-match is a safe assertion here).
        assert_ne!(
            right(&audio, expected_end_sample),
            CUE_AMPLITUDE,
            "{label}: cue must not still be sounding at the downbeat-minus-lead-beats point"
        );
    }

    // Both cues finish the same number of pulses before their own downbeat,
    // regardless of clip length -- the core claim of end-anchored scheduling.
    let verse_end = grid.pulse_to_sample(verse_downbeat_pulse - 4);
    let bridge_end = grid.pulse_to_sample(bridge_downbeat_pulse - 4);
    assert_eq!(
        verse_downbeat_pulse - grid.sample_to_pulse(verse_end),
        bridge_downbeat_pulse - grid.sample_to_pulse(bridge_end),
    );
}
