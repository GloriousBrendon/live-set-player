//! Live-engine coverage for count-in (`docs/SPEC.md` §6) that the offline renderer
//! and the offline/live equivalence suite can't exercise on their own:
//!
//! 1. `Status.count_in_beats_remaining` -- only observable through `RtEngine`'s
//!    status channel, not from a rendered buffer.
//! 2. A mid-song rehearsal start (arming a section other than the first, then Play)
//!    must be bit-identical to the offline render of that section alone with the
//!    same count-in -- the seam the render-start-position change is most likely to
//!    break, extended to the specific case §6 calls out ("also applies when starting
//!    from a section mid-song").

use lsp_engine::click::hit_length_samples;
use lsp_engine::path::RelPath;
use lsp_engine::project::{
    AudioFileRef, Bus, BusLayout, ClickConfig, DownmixMode, Project, Section, Song, Track,
    TrackKind,
};
use lsp_engine::render::{self, AudioBank, RenderOptions};
use lsp_engine::rt::{self, Command, TransportState};
use lsp_engine::sections::PerformanceEntry;
use lsp_engine::timeline::{Grid, TimeSignature};

const RATE: u32 = 48000;
const BPM: f64 = 178.0;

fn project() -> Project {
    Project {
        schema_version: lsp_engine::project::SCHEMA_VERSION,
        name: "count-in test".into(),
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

/// Intro(1-4) Verse(5-12) Chorus(13-20, loopable) Bridge(21-24), 4/4 @ 178. The
/// song's own `count_in_bars` (1) is deliberately left at the default -- these tests
/// send an explicit `SetCountInOverride` so the count-in length under test doesn't
/// depend on the fixture.
fn song() -> Song {
    Song {
        id: "song1".into(),
        title: "Count-in Test Song".into(),
        bpm: BPM,
        time_signature: TimeSignature::FOUR_FOUR,
        offset_samples: 0,
        count_in_bars: 1,
        accent_pattern: vec![],
        sections: vec![
            section("Intro", 1, 4, false),
            section("Verse", 5, 8, false),
            section("Chorus", 13, 8, true),
            section("Bridge", 21, 4, false),
        ],
        tracks: vec![backtrack_track()],
        disabled: false,
    }
}

fn grid() -> Grid {
    Grid::new(RATE, BPM, TimeSignature::FOUR_FOUR).unwrap()
}

/// Ramp stub (not silence): a leaked track read during count-in would decode to a
/// wrong nonzero source frame instead of coincidentally matching silence.
fn bank() -> AudioBank {
    let g = grid();
    let frames = (g.bar_to_pulse(30) as f64 * g.samples_per_pulse()).ceil() as usize;
    let mut bank = AudioBank::new();
    bank.insert("bt", render::ramp_stub(frames));
    bank
}

/// Rehearsal flow (stop, arm a mid-song section, Play) with a count-in: the beats-
/// remaining counter starts at the full count-in length, counts down monotonically,
/// no section is reported active while it's running, `state` never leaves `Playing`
/// (guards the bug where "no active entry" during a count-in could be mistaken for
/// natural end-of-song), and the armed section (not the first one in the song)
/// becomes active exactly when the counter clears.
#[test]
fn count_in_beats_remaining_counts_down_from_a_mid_song_rehearsal_start() {
    let song = song();
    let bank = bank();
    let (mut engine, mut handle, mut garbage) = rt::new_engine(RATE, false);
    let loaded = rt::prepare_loaded(&project(), &song, &bank, RATE).unwrap();
    let count_in_bars = 2u32;

    handle.send(Command::LoadSong(loaded)).ok().unwrap();
    // Rehearsal flow: stop, arm a mid-song section (the Chorus), play -- count-in
    // must apply here exactly as it would from the top of the song (§6).
    handle.send(Command::SeekToSection(2)).ok().unwrap();
    handle
        .send(Command::SetCountInOverride(Some(count_in_bars)))
        .ok()
        .unwrap();
    handle.send(Command::Play).ok().unwrap();

    let g = grid();
    let count_in_pulses = count_in_bars as i64 * g.pulses_per_bar();
    let count_in_samples = g.pulse_to_sample(count_in_pulses) as usize;

    const CHANNELS: usize = 2;
    const BLOCK: usize = 64; // small blocks so the countdown is observed at fine grain
    let mut buf = vec![0.0f32; BLOCK * CHANNELS];

    let mut last_remaining: Option<u32> = None;
    let mut saw_full_count = false;
    let mut downbeat_frame: Option<usize> = None;
    let mut rendered = 0usize;

    while downbeat_frame.is_none() && rendered < count_in_samples + BLOCK * 100 {
        engine.process(&mut buf, CHANNELS);
        rendered += BLOCK;

        let st = handle.latest_status().unwrap();
        assert_eq!(
            st.state,
            TransportState::Playing,
            "must stay Playing through the count-in, never mistake it for natural end"
        );

        match st.count_in_beats_remaining {
            Some(n) => {
                if n == count_in_pulses as u32 {
                    saw_full_count = true;
                }
                if let Some(prev) = last_remaining {
                    assert!(n <= prev, "count-in beats remaining must not increase");
                }
                last_remaining = Some(n);
                assert_eq!(st.section, -1, "no section active while counting in");
            }
            None if last_remaining.is_some() => {
                downbeat_frame = Some(rendered);
                assert_eq!(
                    st.section, 2,
                    "Chorus must become active exactly when the count-in ends"
                );
            }
            None => {}
        }
    }

    assert!(
        saw_full_count,
        "never observed the count-in's starting beat count"
    );
    assert!(downbeat_frame.is_some(), "count-in never ended");
    // Sanity: the downbeat landed close to the grid-predicted count-in length --
    // within one block's worth of the block-granular status polling used here.
    let observed = downbeat_frame.unwrap() as i64;
    assert!(
        (observed - count_in_samples as i64).abs() <= BLOCK as i64,
        "downbeat landed at frame {observed}, expected near {count_in_samples}"
    );
    garbage.drain();
}

/// A mid-song rehearsal start (armed Bridge, not bar 1) with a nonzero count-in must
/// be bit-identical to the offline render of the Bridge alone with the same count-in
/// -- the equivalence guarantee `rt_equivalence.rs` proves for a fresh-from-bar-1
/// count-in, extended to the specific case §6 calls out. Bridge (not the loopable
/// Chorus) so the comparison window can end cleanly at natural end-of-song on both
/// sides, with no loop wrap to release first.
#[test]
fn live_count_in_from_armed_mid_song_section_matches_offline_render() {
    let song = song();
    let bank = bank();
    let count_in_bars = 2u32;
    let bridge_index = 3;

    let order = [PerformanceEntry::once(bridge_index)];
    let hit_len = hit_length_samples(RATE, 40.0);
    let offline = render::render_song(
        &project(),
        &song,
        &order,
        &bank,
        &RenderOptions {
            lead_in_samples: 0,
            tail_samples: hit_len as u64,
            count_in_bars,
        },
    )
    .unwrap()
    .interleaved;

    let (mut engine, mut handle, mut garbage) = rt::new_engine(RATE, false);
    let loaded = rt::prepare_loaded(&project(), &song, &bank, RATE).unwrap();
    handle.send(Command::LoadSong(loaded)).ok().unwrap();
    handle
        .send(Command::SeekToSection(bridge_index))
        .ok()
        .unwrap();
    handle
        .send(Command::SetCountInOverride(Some(count_in_bars)))
        .ok()
        .unwrap();
    handle.send(Command::Play).ok().unwrap();

    const CHANNELS: usize = 2;
    let frames_total = offline.len() / 2;
    let mut live = Vec::with_capacity(offline.len());
    let mut buf = vec![0.0f32; 733 * CHANNELS]; // deliberately awkward block size
    let mut rendered = 0usize;
    while rendered < frames_total {
        let n = 733.min(frames_total - rendered);
        let chunk = &mut buf[..n * CHANNELS];
        engine.process(chunk, CHANNELS);
        live.extend_from_slice(chunk);
        rendered += n;
        let _ = handle.latest_status();
    }

    assert_eq!(live.len(), offline.len());
    for (i, (l, o)) in live.iter().zip(offline.iter()).enumerate() {
        assert!(
            l == o,
            "mismatch at interleaved index {i} (frame {}, ch {}): live {l}, offline {o}",
            i / 2,
            i % 2
        );
    }
    garbage.drain();
}
