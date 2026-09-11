//! The phase-2 contract tests:
//!
//! 1. **Offline == live.** The offline renderer (`render::render_song`) and the
//!    real-time engine (`rt::RtEngine::process`, driven headlessly) must produce
//!    **bit-identical** interleaved output for equivalent programmes — including
//!    seamless looping, manual advance at a bar boundary, mid-section truncation,
//!    and seek. Both run the same `PlaybackCore`; these tests prove the two drivers
//!    steer it identically.
//! 2. **Block-size invariance.** The same programme rendered with different (and
//!    irregular) block sizes is bit-identical, which is what makes "offline == live"
//!    independent of the device's buffer size.
//! 3. **Starvation is defined behaviour.** The transport advances by exactly the
//!    frames rendered and nothing else (no wall-clock anywhere), so skipped/late/
//!    resized callbacks — xruns, from the engine's point of view — delay output in
//!    wall time but can never shift the click against the backtrack, corrupt
//!    transport state, or drift positions. Asserted, not assumed.

use lsp_engine::path::RelPath;
use lsp_engine::project::{
    AudioFileRef, Bus, BusLayout, ClickConfig, CueConfig, DownmixMode, Project, Section, Song,
    Track, TrackKind,
};
use lsp_engine::render::{self, AudioBank, CueBank, RenderOptions};
use lsp_engine::rt::{self, Command, TransportState};
use lsp_engine::sections::PerformanceEntry;
use lsp_engine::timeline::{Grid, TimeSignature};

const RATE: u32 = 48000;
const BPM: f64 = 178.0;

fn project() -> Project {
    Project {
        schema_version: lsp_engine::project::SCHEMA_VERSION,
        name: "RT test".into(),
        sample_rate: RATE,
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

fn section(name: &str, start_bar: u32, length_bars: u32, loopable: bool) -> Section {
    Section {
        name: name.into(),
        start_bar,
        length_bars,
        loopable,
        cue_text: None,
        cue_lead_beats: 4,
    }
}

/// Intro(bars 1-4) Verse(5-12) Chorus(13-20, loopable) Bridge(21-24), 4/4 @ 178.
fn four_section_song(offset_samples: i64) -> Song {
    Song {
        id: "song1".into(),
        title: "RT Test Song".into(),
        bpm: BPM,
        time_signature: TimeSignature::FOUR_FOUR,
        offset_samples,
        count_in_bars: 1,
        accent_pattern: vec![],
        sections: vec![
            section("Intro", 1, 4, false),
            section("Verse", 5, 8, false),
            section("Chorus", 13, 8, true),
            section("Bridge", 21, 4, false),
        ],
        tracks: vec![backtrack_track()],
        auto_continue: false,
        disabled: false,
    }
}

fn grid() -> Grid {
    Grid::new(RATE, BPM, TimeSignature::FOUR_FOUR).unwrap()
}

fn bank_for(song: &Song) -> AudioBank {
    // Ramp stub: every sample encodes its own source frame index, so any source
    // mapping error (wrong section, wrong offset, spurious or missing crossfade)
    // shows up as a hard value mismatch rather than silence-vs-silence.
    let g = grid();
    let max_bar = song
        .sections
        .iter()
        .map(|s| s.start_bar + s.length_bars)
        .max()
        .unwrap() as i64;
    let frames = (g.bar_to_pulse(max_bar + 4) as f64 * g.samples_per_pulse()).ceil() as usize
        + song.offset_samples.max(0) as usize;
    let mut bank = AudioBank::new();
    bank.insert("bt", render::ramp_stub(frames));
    bank
}

/// Drive the engine headlessly: process `frames_total` frames using the block sizes
/// yielded by `blocks` (cycled), sending each `(at_frame, command)` pair the first
/// time the rendered-frame count reaches `at_frame`. Returns the concatenated
/// stereo-interleaved output.
fn drive_engine(
    engine: &mut rt::RtEngine,
    handle: &mut rt::EngineHandle,
    frames_total: usize,
    blocks: &[usize],
    commands: &mut Vec<(usize, Command)>,
) -> Vec<f32> {
    const CHANNELS: usize = 2;
    let mut out = Vec::with_capacity(frames_total * CHANNELS);
    let mut buf = vec![0.0f32; blocks.iter().copied().max().unwrap() * CHANNELS];
    let mut rendered = 0usize;
    let mut block_idx = 0usize;
    while rendered < frames_total {
        // Deliver any command whose trigger frame has been reached.
        while let Some(pos) = commands.iter().position(|(at, _)| *at <= rendered) {
            let (_, cmd) = commands.remove(pos);
            handle.send(cmd).ok().expect("command queue full");
        }
        let n = blocks[block_idx % blocks.len()].min(frames_total - rendered);
        block_idx += 1;
        if n == 0 {
            continue;
        }
        let chunk = &mut buf[..n * CHANNELS];
        engine.process(chunk, CHANNELS);
        out.extend_from_slice(chunk);
        rendered += n;
        // Poll like a real UI would: the status queue drops snapshots when full
        // (by design — the UI only ever wants the latest), so a consumer that
        // never drains would read arbitrarily stale state.
        let _ = handle.latest_status();
    }
    out
}

fn assert_identical(live: &[f32], offline: &[f32], what: &str) {
    assert_eq!(live.len(), offline.len(), "{what}: length mismatch");
    for (i, (l, o)) in live.iter().zip(offline.iter()).enumerate() {
        assert!(
            l == o,
            "{what}: first mismatch at interleaved index {i} (frame {}, ch {}): live {l}, offline {o}",
            i / 2,
            i % 2
        );
    }
}

fn hit_len() -> i64 {
    lsp_engine::click::hit_length_samples(RATE, 40.0)
}

fn offline_reference(song: &Song, order: &[PerformanceEntry], bank: &AudioBank) -> Vec<f32> {
    let audio = render::render_song(
        &project(),
        song,
        order,
        bank,
        &CueBank::new(),
        &RenderOptions {
            lead_in_samples: 0,
            tail_samples: hit_len() as u64,
            count_in_bars: 0,
        },
    )
    .unwrap();
    audio.interleaved
}

fn perf_bar_sample(bar: i64) -> usize {
    let g = grid();
    g.pulse_to_sample(g.bar_to_pulse(bar)) as usize
}

// -----------------------------------------------------------------------------------
// 1. Offline == live
// -----------------------------------------------------------------------------------

/// Natural playthrough with the loopable Chorus repeating 3 times, released by a
/// manual advance sent during the final bar of the third repeat (so the advance
/// lands exactly on the repeat's end boundary — no truncation). Live output must be
/// bit-identical to the offline render of [Intro, Verse, Chorus×3, Bridge].
#[test]
fn live_loop_and_advance_matches_offline_render() {
    let song = four_section_song(2400);
    let bank = bank_for(&song);

    let order = [
        PerformanceEntry::once(0),
        PerformanceEntry::once(1),
        PerformanceEntry {
            section_index: 2,
            repeats: 3,
        },
        PerformanceEntry::once(3),
    ];
    let offline = offline_reference(&song, &order, &bank);

    let (mut engine, mut handle, mut garbage) = rt::new_engine(RATE, false);
    // These tests are about loop/advance/seek/starvation exactness, not
    // count-in (that gets its own dedicated equivalence test below) — disable it
    // so `Play` behaves exactly as it did before count-in existed.
    handle
        .send(Command::SetCountInOverride(Some(0)))
        .ok()
        .unwrap();
    let loaded = rt::prepare_loaded(&project(), &song, &bank, &CueBank::new(), RATE).unwrap();
    // Perf bars: Intro 0-3, Verse 4-11, Chorus repeats 12-19 / 20-27 / 28-35,
    // Bridge 36-39. Advance during perf bar 35 (last bar of the third repeat).
    let advance_at = perf_bar_sample(35) + 1000;
    let mut commands = vec![
        (0usize, Command::LoadSong(loaded)),
        (0usize, Command::Play),
        (advance_at, Command::AdvanceSection),
    ];
    let frames_total = offline.len() / 2;
    let live = drive_engine(
        &mut engine,
        &mut handle,
        frames_total,
        &[1024],
        &mut commands,
    );

    assert_identical(&live, &offline, "loop+advance programme");
    let status = handle.latest_status().unwrap();
    assert_eq!(
        status.state,
        TransportState::Stopped,
        "must stop after tail"
    );
    garbage.drain();
}

/// Manual advance mid-section: sent during the Verse's second bar, so the Verse is
/// truncated at the next bar boundary (2 bars played) and the Chorus takes over
/// through a crossfade. Offline equivalent: the same song with a 2-bar Verse.
/// Also exercises the queued-section status between trigger and boundary.
#[test]
fn live_mid_section_advance_truncates_at_bar_boundary() {
    let song = four_section_song(0);
    let bank = bank_for(&song);

    // Offline reference: identical song except Verse is 2 bars long (same source
    // start), played [Intro, VerseTrunc, Chorus, Bridge] — Chorus released by an
    // end-of-repeat advance so it plays once.
    let mut song_ref = song.clone();
    song_ref.sections[1].length_bars = 2;
    let order = [
        PerformanceEntry::once(0),
        PerformanceEntry::once(1),
        PerformanceEntry::once(2),
        PerformanceEntry::once(3),
    ];
    let offline = offline_reference(&song_ref, &order, &bank);

    let (mut engine, mut handle, mut garbage) = rt::new_engine(RATE, false);
    // These tests are about loop/advance/seek/starvation exactness, not
    // count-in (that gets its own dedicated equivalence test below) — disable it
    // so `Play` behaves exactly as it did before count-in existed.
    handle
        .send(Command::SetCountInOverride(Some(0)))
        .ok()
        .unwrap();
    let loaded = rt::prepare_loaded(&project(), &song, &bank, &CueBank::new(), RATE).unwrap();
    // Live perf bars: Intro 0-3, Verse from bar 4; advance lands mid bar 5 (the
    // Verse's second bar) => boundary at perf bar 6, Verse truncated to 2 bars.
    // Chorus then spans perf bars 6-13; advance in its last bar (13) releases it
    // after one repeat. Bridge spans 14-17.
    let advance_verse_at = perf_bar_sample(5) + 3000;
    let advance_chorus_at = perf_bar_sample(13) + 3000;
    let mut commands = vec![
        (0usize, Command::LoadSong(loaded)),
        (0usize, Command::Play),
        (advance_verse_at, Command::AdvanceSection),
        (advance_chorus_at, Command::AdvanceSection),
    ];
    let frames_total = offline.len() / 2;

    // Drive up to just past the first advance to check the queued indicator, then
    // continue to the end.
    const CHANNELS: usize = 2;
    let mut live = Vec::with_capacity(frames_total * CHANNELS);
    let mut buf = vec![0.0f32; 1024 * CHANNELS];
    let mut rendered = 0usize;
    let mut checked_queued = false;
    while rendered < frames_total {
        while let Some(pos) = commands.iter().position(|(at, _)| *at <= rendered) {
            let (_, cmd) = commands.remove(pos);
            handle.send(cmd).ok().expect("command queue full");
        }
        let n = 1024.min(frames_total - rendered);
        let chunk = &mut buf[..n * CHANNELS];
        engine.process(chunk, CHANNELS);
        live.extend_from_slice(chunk);
        rendered += n;
        let latest = handle.latest_status();

        // Between the advance trigger and the bar-6 boundary, the queued section
        // must be visible in the status (§7).
        if !checked_queued && rendered > advance_verse_at + 1024 && rendered < perf_bar_sample(6) {
            let st = latest.unwrap();
            assert_eq!(
                st.queued,
                rt::QueuedStatus::Section(2),
                "queued advance must be visible before the boundary"
            );
            assert_eq!(st.section, 1, "still in the Verse until the boundary");
            checked_queued = true;
        }
    }
    assert!(checked_queued, "test never observed the queued state");

    assert_identical(&live, &offline, "mid-section truncating advance");
    garbage.drain();
}

/// Seek while playing: from inside the Intro's second bar straight to the Bridge,
/// quantised to the next bar boundary. Offline equivalent: 2-bar Intro, then Bridge.
#[test]
fn live_seek_matches_offline_render() {
    let song = four_section_song(0);
    let bank = bank_for(&song);

    let mut song_ref = song.clone();
    song_ref.sections[0].length_bars = 2;
    let order = [PerformanceEntry::once(0), PerformanceEntry::once(3)];
    let offline = offline_reference(&song_ref, &order, &bank);

    let (mut engine, mut handle, mut garbage) = rt::new_engine(RATE, false);
    // These tests are about loop/advance/seek/starvation exactness, not
    // count-in (that gets its own dedicated equivalence test below) — disable it
    // so `Play` behaves exactly as it did before count-in existed.
    handle
        .send(Command::SetCountInOverride(Some(0)))
        .ok()
        .unwrap();
    let loaded = rt::prepare_loaded(&project(), &song, &bank, &CueBank::new(), RATE).unwrap();
    let seek_at = perf_bar_sample(1) + 2000; // inside Intro bar 2 => boundary at bar 2
    let mut commands = vec![
        (0usize, Command::LoadSong(loaded)),
        (0usize, Command::Play),
        (seek_at, Command::SeekToSection(3)),
    ];
    let frames_total = offline.len() / 2;
    let live = drive_engine(
        &mut engine,
        &mut handle,
        frames_total,
        &[1024],
        &mut commands,
    );

    assert_identical(&live, &offline, "quantised seek");
    garbage.drain();
}

/// Seek while stopped arms the section: Play starts a fresh performance timeline
/// there (rehearsal flow: stop, pick a section, play). Offline equivalent: the
/// programme starting at the Chorus.
#[test]
fn arm_section_then_play_starts_fresh_timeline_there() {
    let song = four_section_song(2400);
    let bank = bank_for(&song);

    // Chorus once (released by advance in its last bar), then Bridge.
    let order = [PerformanceEntry::once(2), PerformanceEntry::once(3)];
    let offline = offline_reference(&song, &order, &bank);

    let (mut engine, mut handle, mut garbage) = rt::new_engine(RATE, false);
    // These tests are about loop/advance/seek/starvation exactness, not
    // count-in (that gets its own dedicated equivalence test below) — disable it
    // so `Play` behaves exactly as it did before count-in existed.
    handle
        .send(Command::SetCountInOverride(Some(0)))
        .ok()
        .unwrap();
    let loaded = rt::prepare_loaded(&project(), &song, &bank, &CueBank::new(), RATE).unwrap();
    // Chorus spans perf bars 0-7 in the fresh timeline; advance during bar 7.
    let advance_at = perf_bar_sample(7) + 500;
    let mut commands = vec![
        (0usize, Command::LoadSong(loaded)),
        (0usize, Command::SeekToSection(2)), // stopped => arms
        (0usize, Command::Play),
        (advance_at, Command::AdvanceSection),
    ];
    let frames_total = offline.len() / 2;
    let live = drive_engine(
        &mut engine,
        &mut handle,
        frames_total,
        &[1024],
        &mut commands,
    );

    assert_identical(&live, &offline, "armed-section start");
    garbage.drain();
}

/// The count-in itself (§6) shifts the render's start position by a non-integer-
/// samples-per-pulse amount — exactly the kind of seam where offline and live could
/// silently diverge (wrong pending->active promotion, the click's negative-pulse
/// clamp, block-size splitting across the count-in/downbeat boundary). Exercises both
/// the per-song default (song `count_in_bars`) and the global override, at a
/// non-default 2-bar count-in, and drives with an awkward block size so the
/// count-in/downbeat seam isn't always landing on a block boundary.
///
/// The programme still includes the loopable Chorus, repeated twice — offline via
/// `PerformanceEntry { repeats: 2, .. }` (§7's finite stand-in for "loops until
/// advance", now enforced by `resolve_order` for exactly this reason: see
/// `RepeatsOnNonLoopableSection`), live via an `AdvanceSection` sent during the
/// second repeat's last bar. Count-in only shifts *when* content starts, never what
/// the content is, so this must stay bit-identical with the loop released at the
/// same point `live_loop_and_advance_matches_offline_render` proves for the
/// no-count-in case — this is that same case, with a count-in ahead of it.
#[test]
fn live_count_in_matches_offline_render_with_a_loopable_section_released_by_advance() {
    let song = four_section_song(2400);
    let bank = bank_for(&song);
    let count_in_bars = 2u32; // overrides the song's own count_in_bars (1)
    let chorus_repeats = 2u32;

    let order = [
        PerformanceEntry::once(0),
        PerformanceEntry::once(1),
        PerformanceEntry {
            section_index: 2,
            repeats: chorus_repeats,
        },
        PerformanceEntry::once(3),
    ];
    let offline = {
        let audio = render::render_song(
            &project(),
            &song,
            &order,
            &bank,
            &CueBank::new(),
            &RenderOptions {
                lead_in_samples: 0,
                tail_samples: hit_len() as u64,
                count_in_bars,
            },
        )
        .unwrap();
        audio.interleaved
    };

    let (mut engine, mut handle, mut garbage) = rt::new_engine(RATE, false);
    let loaded = rt::prepare_loaded(&project(), &song, &bank, &CueBank::new(), RATE).unwrap();
    let g = grid();
    let count_in_samples = g.pulse_to_sample(count_in_bars as i64 * g.pulses_per_bar());
    // Chorus's perf start is bar 12 (after Intro's 4 + Verse's 8); its second (last)
    // repeat's final bar is 12 + 8*chorus_repeats - 1 = 27. `perf_bar_sample` is
    // performance-time-relative (unaffected by count-in — count-in only prepends
    // negative-time content, it never shifts the content's own bar positions), but
    // `drive_engine`'s command triggers are output-frame-relative, i.e. relative to
    // Play, which now starts `count_in_samples` before performance sample 0 — so the
    // trigger frame needs that offset added back in.
    let advance_at = (perf_bar_sample(12 + 8 * chorus_repeats as i64 - 1) as i64
        + 1000
        + count_in_samples) as usize;
    let mut commands = vec![
        (0usize, Command::SetCountInOverride(Some(count_in_bars))),
        (0usize, Command::LoadSong(loaded)),
        (0usize, Command::Play),
        (advance_at, Command::AdvanceSection),
    ];
    let frames_total = offline.len() / 2;
    let live = drive_engine(
        &mut engine,
        &mut handle,
        frames_total,
        &[733],
        &mut commands,
    );

    assert_identical(&live, &offline, "count-in + loopable-section programme");
    let status = handle.latest_status().unwrap();
    assert_eq!(
        status.count_in_beats_remaining, None,
        "count-in must have cleared by the end of the render"
    );
    garbage.drain();
}

// -----------------------------------------------------------------------------------
// 2. Block-size invariance
// -----------------------------------------------------------------------------------

/// The same programme rendered with 512-frame blocks, awkward 733-frame blocks, and
/// a mixed pattern including 1-frame calls must be bit-identical. This is the
/// property that makes the offline/live guarantee independent of device buffer size.
#[test]
fn output_is_invariant_across_block_sizes() {
    let song = four_section_song(2400);
    let bank = bank_for(&song);
    let advance_at = perf_bar_sample(35) + 1000; // release the 3x-looped Chorus

    let g = grid();
    let content_end = g.pulse_to_sample(g.bar_to_pulse(40)) as usize; // 40 perf bars
    let frames_total = content_end + hit_len() as usize;

    let mut outputs = Vec::new();
    for blocks in [
        vec![512usize],
        vec![733usize],
        vec![4096usize, 1, 257, 1024],
    ] {
        let (mut engine, mut handle, mut garbage) = rt::new_engine(RATE, false);
        // These tests are about loop/advance/seek/starvation exactness, not
        // count-in (that gets its own dedicated equivalence test below) — disable it
        // so `Play` behaves exactly as it did before count-in existed.
        handle
            .send(Command::SetCountInOverride(Some(0)))
            .ok()
            .unwrap();
        let loaded = rt::prepare_loaded(&project(), &song, &bank, &CueBank::new(), RATE).unwrap();
        let mut commands = vec![
            (0usize, Command::LoadSong(loaded)),
            (0usize, Command::Play),
            (advance_at, Command::AdvanceSection),
        ];
        outputs.push(drive_engine(
            &mut engine,
            &mut handle,
            frames_total,
            &blocks,
            &mut commands,
        ));
        garbage.drain();
    }
    assert_identical(&outputs[1], &outputs[0], "733 vs 512 blocks");
    assert_identical(&outputs[2], &outputs[0], "mixed vs 512 blocks");
}

// -----------------------------------------------------------------------------------
// 3. Starvation / xrun semantics: defined, not discovered
// -----------------------------------------------------------------------------------

/// Simulated starvation: the host stops calling the engine for a stretch (as WASAPI
/// does across an underrun — missed periods are simply never rendered), then resumes
/// with a burst of catch-up callbacks of varying size. Defined behaviour:
///
/// - The transport advances by exactly the frames rendered — after starvation the
///   output stream continues from precisely where it left off, so the click can
///   never shift against the backtrack (they share the sample clock).
/// - Commands sent *during* the gap apply at the next real callback, quantised to
///   the bar grid by sample position, not wall time.
/// - The concatenated output is bit-identical to an unstarved run.
#[test]
fn starved_and_bursty_callbacks_do_not_corrupt_transport() {
    let song = four_section_song(2400);
    let bank = bank_for(&song);
    let advance_at = perf_bar_sample(35) + 1000;

    let g = grid();
    let frames_total = g.pulse_to_sample(g.bar_to_pulse(40)) as usize + hit_len() as usize;

    // Reference: steady 1024-frame callbacks.
    let reference = {
        let (mut engine, mut handle, mut garbage) = rt::new_engine(RATE, false);
        // These tests are about loop/advance/seek/starvation exactness, not
        // count-in (that gets its own dedicated equivalence test below) — disable it
        // so `Play` behaves exactly as it did before count-in existed.
        handle
            .send(Command::SetCountInOverride(Some(0)))
            .ok()
            .unwrap();
        let loaded = rt::prepare_loaded(&project(), &song, &bank, &CueBank::new(), RATE).unwrap();
        let mut commands = vec![
            (0usize, Command::LoadSong(loaded)),
            (0usize, Command::Play),
            (advance_at, Command::AdvanceSection),
        ];
        let out = drive_engine(
            &mut engine,
            &mut handle,
            frames_total,
            &[1024],
            &mut commands,
        );
        garbage.drain();
        out
    };

    // Starved run: normal callbacks, then a "gap" (no calls at all — during a real
    // xrun the callback simply doesn't run), during which the advance command
    // arrives; then a catch-up burst of large and tiny callbacks.
    let (mut engine, mut handle, mut garbage) = rt::new_engine(RATE, false);
    // These tests are about loop/advance/seek/starvation exactness, not
    // count-in (that gets its own dedicated equivalence test below) — disable it
    // so `Play` behaves exactly as it did before count-in existed.
    handle
        .send(Command::SetCountInOverride(Some(0)))
        .ok()
        .unwrap();
    let loaded = rt::prepare_loaded(&project(), &song, &bank, &CueBank::new(), RATE).unwrap();
    handle.send(Command::LoadSong(loaded)).ok().unwrap();
    handle.send(Command::Play).ok().unwrap();

    const CHANNELS: usize = 2;
    let mut live = Vec::with_capacity(frames_total * CHANNELS);
    let mut buf = vec![0.0f32; 8192 * CHANNELS];
    let mut rendered = 0usize;
    let mut advance_sent = false;
    // Callback pattern: steady until the advance point, then the host "goes away"
    // (represented by nothing — no state is touched), the command arrives mid-gap,
    // and rendering resumes with a burst: 8192, 3, 8192, 555, then steady 1024.
    let burst = [8192usize, 3, 8192, 555];
    let mut burst_idx = 0usize;
    while rendered < frames_total {
        if !advance_sent && rendered >= advance_at {
            // The gap: the engine sees no callbacks while the user hits advance.
            handle.send(Command::AdvanceSection).ok().unwrap();
            advance_sent = true;
        }
        let n = if advance_sent && burst_idx < burst.len() {
            let n = burst[burst_idx];
            burst_idx += 1;
            n
        } else {
            1024
        }
        .min(frames_total - rendered);
        let chunk = &mut buf[..n * CHANNELS];
        engine.process(chunk, CHANNELS);
        live.extend_from_slice(chunk);
        rendered += n;

        // Transport position must always equal frames rendered: the engine has no
        // other clock to drift against.
        let st = handle.latest_status().unwrap();
        if st.state == TransportState::Playing {
            assert_eq!(
                st.perf_sample, rendered as i64,
                "transport must advance by exactly the frames rendered"
            );
        }
    }

    assert_identical(&live, &reference, "starved/bursty run vs steady run");
    let st = handle.latest_status().unwrap();
    assert_eq!(
        st.state,
        TransportState::Stopped,
        "must recover to a clean stop"
    );
    garbage.drain();
}

// -----------------------------------------------------------------------------------
// Transport odds and ends that equivalence can't express
// -----------------------------------------------------------------------------------

/// Stop ramps to silence within ~10 ms and panic within ~5 ms; both end in Stopped
/// with the output actually silent (no hard cut, no lingering audio).
#[test]
fn stop_and_panic_ramp_to_silence() {
    for (cmd, ramp_ms) in [(Command::Stop, 10.0f64), (Command::PanicStop, 5.0)] {
        let song = four_section_song(0);
        let bank = bank_for(&song);
        let (mut engine, mut handle, mut garbage) = rt::new_engine(RATE, false);
        // These tests are about loop/advance/seek/starvation exactness, not
        // count-in (that gets its own dedicated equivalence test below) — disable it
        // so `Play` behaves exactly as it did before count-in existed.
        handle
            .send(Command::SetCountInOverride(Some(0)))
            .ok()
            .unwrap();
        let loaded = rt::prepare_loaded(&project(), &song, &bank, &CueBank::new(), RATE).unwrap();
        handle.send(Command::LoadSong(loaded)).ok().unwrap();
        handle.send(Command::Play).ok().unwrap();

        const CHANNELS: usize = 2;
        let mut buf = vec![0.0f32; 512 * CHANNELS];
        // Run for a bit, then stop.
        for _ in 0..20 {
            engine.process(&mut buf, CHANNELS);
        }
        handle.send(cmd).ok().unwrap();
        let ramp_frames = (ramp_ms / 1000.0 * RATE as f64).ceil() as usize;
        let mut post = Vec::new();
        for _ in 0..(ramp_frames / 512 + 3) {
            engine.process(&mut buf, CHANNELS);
            post.extend_from_slice(&buf);
        }
        let st = handle.latest_status().unwrap();
        assert_eq!(st.state, TransportState::Stopped);
        // Everything after the ramp window must be exactly silent.
        let after = &post[(ramp_frames + 512) * CHANNELS..];
        assert!(
            after.iter().all(|&s| s == 0.0),
            "output not silent after the stop ramp"
        );
        garbage.drain();
    }
}

/// A looping section loops indefinitely until released, and every wrap is a splice
/// (the ramp-decoded left channel must re-read the section's source start after the
/// crossfade window at each wrap).
#[test]
fn loopable_section_loops_until_advanced() {
    let song = four_section_song(0);
    let bank = bank_for(&song);
    let (mut engine, mut handle, mut garbage) = rt::new_engine(RATE, false);
    // These tests are about loop/advance/seek/starvation exactness, not
    // count-in (that gets its own dedicated equivalence test below) — disable it
    // so `Play` behaves exactly as it did before count-in existed.
    handle
        .send(Command::SetCountInOverride(Some(0)))
        .ok()
        .unwrap();
    let loaded = rt::prepare_loaded(&project(), &song, &bank, &CueBank::new(), RATE).unwrap();
    let mut commands = vec![
        (0usize, Command::LoadSong(loaded)),
        (0usize, Command::SeekToSection(2)), // arm the Chorus
        (0usize, Command::Play),
    ];
    // 5 chorus lengths (8 bars each) — it must still be looping, nothing else runs.
    let frames_total = perf_bar_sample(40);
    let live = drive_engine(
        &mut engine,
        &mut handle,
        frames_total,
        &[1024],
        &mut commands,
    );

    let g = grid();
    let chorus_src_start = g.pulse_to_sample(g.bar_to_pulse(12)); // source bar0 12
    let fade = lsp_engine::crossfade::crossfade_length_samples(RATE, None);
    for wrap in 1..5i64 {
        let wrap_sample = perf_bar_sample(8 * wrap);
        // Just after the crossfade at each wrap, the backtrack reads from the
        // chorus's source start again.
        let idx = wrap_sample + fade as usize + 10;
        let decoded = render::decode_ramp_sample(live[idx * 2]);
        let expected = chorus_src_start + (fade + 10);
        assert_eq!(
            decoded, expected,
            "wrap {wrap}: expected source frame {expected}, decoded {decoded}"
        );
    }
    let st = handle.latest_status().unwrap();
    assert_eq!(st.state, TransportState::Playing, "still looping");
    assert_eq!(st.section, 2);
    garbage.drain();
}
