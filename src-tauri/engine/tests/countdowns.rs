//! Time-based countdown coverage (`docs/SPEC.md` §9.1).
//!
//! The performance view shows these in large type, so the two failure modes that
//! matter are (a) the arithmetic silently disagreeing with the sample clock and
//! (b) counting down to a boundary that is going to move. Both are checked here
//! against `RtEngine`'s status channel, the only place they're observable.

use lsp_engine::path::RelPath;
use lsp_engine::project::{
    AudioFileRef, Bus, BusLayout, ClickConfig, CueConfig, DownmixMode, Project, Section, Song,
    Track, TrackKind,
};
use lsp_engine::render::{self, AudioBank, CueBank};
use lsp_engine::rt::{self, Command};
use lsp_engine::timeline::{Grid, TimeSignature};

const RATE: u32 = 48000;
/// Deliberately non-integer samples-per-pulse (16179.775 @ 48 kHz), per invariant 2.
const BPM: f64 = 178.0;

fn project() -> Project {
    Project {
        schema_version: lsp_engine::project::SCHEMA_VERSION,
        name: "countdown test".into(),
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

/// Intro(4) Verse(8) Chorus(8, loopable) Bridge(4) = 24 performance bars. The
/// loopable Chorus sits in the middle, so sections 0..=2 have an unknowable song
/// end and only the Bridge can report one.
fn song() -> Song {
    Song {
        id: "song1".into(),
        title: "Countdown Test Song".into(),
        bpm: BPM,
        time_signature: TimeSignature::FOUR_FOUR,
        offset_samples: 0,
        count_in_bars: 0,
        accent_pattern: vec![],
        sections: vec![
            section("Intro", 1, 4, false),
            section("Verse", 5, 8, false),
            section("Chorus", 13, 8, true),
            section("Bridge", 21, 4, false),
        ],
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
        auto_continue: false,
        disabled: false,
    }
}

fn grid() -> Grid {
    Grid::new(RATE, BPM, TimeSignature::FOUR_FOUR).unwrap()
}

fn bank() -> AudioBank {
    let g = grid();
    let frames = (g.bar_to_pulse(30) as f64 * g.samples_per_pulse()).ceil() as usize;
    let mut bank = AudioBank::new();
    bank.insert("bt", render::ramp_stub(frames));
    bank
}

/// §9.1: `song_duration_seconds` is the grid's own bar-24 position, and the
/// remaining/position pair stays consistent with the sample clock as it advances --
/// no accumulation, no drift against `perf_sample`.
#[test]
fn song_duration_and_position_track_the_sample_clock() {
    let (mut engine, mut handle, _garbage) = rt::new_engine(RATE, false);
    let loaded = rt::prepare_loaded(&project(), &song(), &bank(), &CueBank::new(), RATE).unwrap();
    handle.send(Command::LoadSong(loaded)).ok().unwrap();
    handle.send(Command::SeekToSection(0)).ok().unwrap();
    handle.send(Command::Play).ok().unwrap();

    let g = grid();
    let expected_duration = g.pulse_to_sample(g.bar_to_pulse(24)) as f64 / RATE as f64;

    const CHANNELS: usize = 2;
    const BLOCK: usize = 256;
    let mut buf = vec![0.0f32; BLOCK * CHANNELS];

    for _ in 0..400 {
        engine.process(&mut buf, CHANNELS);
        let st = handle.latest_status().unwrap();
        if !st.song_loaded || st.section < 0 {
            continue;
        }
        assert!(
            (st.song_duration_seconds - expected_duration).abs() < 1e-9,
            "duration {} != grid-derived {expected_duration}",
            st.song_duration_seconds
        );
        // The position readout is exactly perf_sample in seconds -- it must never be
        // a separately-advanced counter (invariant 2).
        let expected_pos = st.perf_sample as f64 / RATE as f64;
        assert!(
            (st.song_position_seconds - expected_pos).abs() < 1e-9,
            "position {} != perf_sample-derived {expected_pos}",
            st.song_position_seconds
        );
        // Section countdown never goes negative and never exceeds the longest
        // section (8 bars).
        assert!(st.section_seconds_remaining >= 0.0);
        assert!(st.section_seconds_remaining <= 8.0 * 4.0 * 60.0 / BPM + 1e-6);
    }
}

/// §9.1: while a `loopable` section is active *or still ahead*, the song end is not
/// knowable and must be reported as unknown rather than counted down. Only once
/// playback is past the last loopable section does a number appear.
#[test]
fn song_seconds_remaining_is_unknown_until_past_the_last_loopable_section() {
    let (mut engine, mut handle, _garbage) = rt::new_engine(RATE, false);
    let loaded = rt::prepare_loaded(&project(), &song(), &bank(), &CueBank::new(), RATE).unwrap();
    handle.send(Command::LoadSong(loaded)).ok().unwrap();
    // Start on the Bridge (index 3): nothing loopable at or after it.
    handle.send(Command::SeekToSection(3)).ok().unwrap();
    handle.send(Command::Play).ok().unwrap();

    const CHANNELS: usize = 2;
    const BLOCK: usize = 256;
    let mut buf = vec![0.0f32; BLOCK * CHANNELS];

    let mut saw_known = false;
    for _ in 0..200 {
        engine.process(&mut buf, CHANNELS);
        let st = handle.latest_status().unwrap();
        if st.section == 3 {
            let remaining = st
                .song_seconds_remaining
                .expect("Bridge has no loopable section at or after it");
            assert!(remaining >= 0.0);
            saw_known = true;
        }
    }
    assert!(saw_known, "never observed the Bridge active");

    // Now the Chorus (index 2), which is itself loopable: unknown for as long as
    // it is active.
    let (mut engine, mut handle, _garbage) = rt::new_engine(RATE, false);
    let loaded = rt::prepare_loaded(&project(), &song(), &bank(), &CueBank::new(), RATE).unwrap();
    handle.send(Command::LoadSong(loaded)).ok().unwrap();
    handle.send(Command::SeekToSection(2)).ok().unwrap();
    handle.send(Command::Play).ok().unwrap();

    let mut saw_unknown = false;
    for _ in 0..200 {
        engine.process(&mut buf, CHANNELS);
        let st = handle.latest_status().unwrap();
        if st.section == 2 {
            assert!(
                st.song_seconds_remaining.is_none(),
                "a loopable section reported a definite song end"
            );
            saw_unknown = true;
        }
    }
    assert!(saw_unknown, "never observed the Chorus active");
}

/// §9.1: queuing an advance rewrites the active entry's end, so the section
/// countdown must shorten immediately rather than keep counting to the original
/// boundary.
#[test]
fn queued_advance_shortens_the_section_countdown() {
    let (mut engine, mut handle, _garbage) = rt::new_engine(RATE, false);
    let loaded = rt::prepare_loaded(&project(), &song(), &bank(), &CueBank::new(), RATE).unwrap();
    handle.send(Command::LoadSong(loaded)).ok().unwrap();
    // Verse is 8 bars -- long enough that an advance to the next bar boundary is a
    // clear reduction.
    handle.send(Command::SeekToSection(1)).ok().unwrap();
    handle.send(Command::Play).ok().unwrap();

    const CHANNELS: usize = 2;
    const BLOCK: usize = 256;
    let mut buf = vec![0.0f32; BLOCK * CHANNELS];

    // Settle into the Verse and record the countdown before advancing.
    let mut before = None;
    for _ in 0..40 {
        engine.process(&mut buf, CHANNELS);
        let st = handle.latest_status().unwrap();
        if st.section == 1 {
            before = Some(st.section_seconds_remaining);
        }
    }
    let before = before.expect("never entered the Verse");

    handle.send(Command::AdvanceSection).ok().unwrap();
    engine.process(&mut buf, CHANNELS);
    let after = handle.latest_status().unwrap().section_seconds_remaining;

    assert!(
        after < before,
        "countdown did not shorten on a queued advance: {before} -> {after}"
    );
}
