//! CLAUDE.md invariant 1 as a test failure, not a convention: the audio callback
//! (`RtEngine::process`) must never allocate **or deallocate** across thousands of
//! calls — including section transitions, loop wraps, crossfades, queued advances,
//! live gain/mute changes, and a full song swap (`LoadSong`), whose old song must
//! leave via the garbage queue rather than being dropped in place.
//!
//! Mechanism: a counting `#[global_allocator]` wraps the system allocator; the test
//! snapshots the counters, drives several thousand callbacks, and asserts both
//! deltas are zero. Integration tests get their own process, so the global
//! allocator hook doesn't leak into the rest of the suite.
//!
//! Single-threaded by design: the test thread plays both the UI role (pushing
//! commands, popping status) and the audio role (calling `process`), so *any*
//! allocation inside the measured window — even one on the "UI side" of an rtrb
//! queue — fails the test. That is stricter than the invariant requires, which is
//! the right direction.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};

struct CountingAllocator;

static ALLOCS: AtomicU64 = AtomicU64::new(0);
static DEALLOCS: AtomicU64 = AtomicU64::new(0);

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::SeqCst);
        System.alloc(layout)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        DEALLOCS.fetch_add(1, Ordering::SeqCst);
        System.dealloc(ptr, layout)
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // A realloc is both: it may free and allocate. Count it on both sides.
        ALLOCS.fetch_add(1, Ordering::SeqCst);
        DEALLOCS.fetch_add(1, Ordering::SeqCst);
        System.realloc(ptr, layout, new_size)
    }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

use lsp_engine::path::RelPath;
use lsp_engine::project::{
    AudioFileRef, Bus, BusLayout, ClickConfig, CueConfig, DownmixMode, Project, Section, Song,
    Track, TrackKind,
};
use lsp_engine::render::{self, AudioBank, CueBank};
use lsp_engine::rt::{self, Command};
use lsp_engine::timeline::TimeSignature;

const RATE: u32 = 48000;

fn project() -> Project {
    Project {
        schema_version: lsp_engine::project::SCHEMA_VERSION,
        name: "no-alloc".into(),
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

fn song() -> Song {
    Song {
        id: "song1".into(),
        title: "No Alloc".into(),
        bpm: 178.0,
        time_signature: TimeSignature::FOUR_FOUR,
        offset_samples: 2400,
        count_in_bars: 1,
        accent_pattern: vec![],
        sections: vec![
            Section {
                name: "A".into(),
                start_bar: 1,
                length_bars: 4,
                loopable: false,
                cue_text: None,
                cue_lead_beats: 4,
            },
            // Loopable 2-bar section: wraps (and therefore splices + crossfades)
            // every ~65k samples, so a few thousand 512-frame callbacks cross many
            // entry transitions.
            Section {
                name: "Loop".into(),
                start_bar: 5,
                length_bars: 2,
                loopable: true,
                cue_text: None,
                cue_lead_beats: 4,
            },
            Section {
                name: "Out".into(),
                start_bar: 7,
                length_bars: 4,
                loopable: false,
                cue_text: None,
                cue_lead_beats: 4,
            },
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

#[test]
fn process_never_touches_the_allocator() {
    const CHANNELS: usize = 2;
    const BLOCK: usize = 512;
    const CALLS: usize = 6000; // ~64 seconds of audio at 48 kHz

    // ---- Setup (allocation expected and fine here) ----
    let song = song();
    let mut bank = AudioBank::new();
    bank.insert("bt", render::silent_stub(1_500_000));
    let (mut engine, mut handle, mut garbage) = rt::new_engine(RATE, false);
    let first = rt::prepare_loaded(&project(), &song, &bank, &CueBank::new(), RATE).unwrap();
    // A second song, prepared up front, to be swapped in mid-measurement: the swap
    // itself (Box move in, old Box parked on the garbage queue) must not touch the
    // allocator on the audio thread.
    let second = rt::prepare_loaded(&project(), &song, &bank, &CueBank::new(), RATE).unwrap();

    handle.send(Command::LoadSong(first)).ok().unwrap();
    handle.send(Command::SeekToSection(1)).ok().unwrap(); // arm the loop section
    handle.send(Command::Play).ok().unwrap();

    let mut out = vec![0.0f32; BLOCK * CHANNELS];
    // Warm-up: process a few blocks so lazy one-time work (if any) happens outside
    // the measured window.
    for _ in 0..16 {
        engine.process(&mut out, CHANNELS);
        let _ = handle.latest_status();
    }

    // Pre-build the command payload swapped in mid-window.
    let mut pending_swap = Some(Command::LoadSong(second));

    // ---- Measured window ----
    let allocs_before = ALLOCS.load(Ordering::SeqCst);
    let deallocs_before = DEALLOCS.load(Ordering::SeqCst);

    for call in 0..CALLS {
        // Sprinkle live parameter changes and transport commands through the run.
        match call % 500 {
            100 => {
                let _ = handle.send(Command::SetTrackGainDb { track: 0, db: -6.0 });
            }
            200 => {
                let _ = handle.send(Command::SetTrackMuted {
                    track: 0,
                    muted: call % 1000 == 200,
                });
            }
            300 => {
                let _ = handle.send(Command::SetClickGainDb(-3.0));
            }
            _ => {}
        }
        if call == 2000 {
            // Queued advance out of the loop... (truncation + splice machinery)
            let _ = handle.send(Command::AdvanceSection);
        }
        if call == 2100 {
            // ...and straight back in, so the loop keeps wrapping afterwards.
            let _ = handle.send(Command::SeekToSection(1));
        }
        if call == 4000 {
            // Full song swap on the fly. Old song must exit via the garbage queue.
            let _ = handle.send(pending_swap.take().unwrap());
        }
        if call == 4001 {
            let _ = handle.send(Command::SeekToSection(1));
            let _ = handle.send(Command::Play);
        }
        engine.process(&mut out, CHANNELS);
        let _ = handle.latest_status(); // Copy pop; must not allocate either.
    }

    let alloc_delta = ALLOCS.load(Ordering::SeqCst) - allocs_before;
    let dealloc_delta = DEALLOCS.load(Ordering::SeqCst) - deallocs_before;

    // ---- Verdict before any cleanup (cleanup will allocate/deallocate freely) ----
    assert_eq!(
        alloc_delta, 0,
        "audio path performed {alloc_delta} allocation(s) across {CALLS} callbacks"
    );
    assert_eq!(
        dealloc_delta, 0,
        "audio path performed {dealloc_delta} deallocation(s) across {CALLS} callbacks \
         (heap data must leave via the garbage queue, never be dropped in the callback)"
    );

    // The swapped-out song must actually be sitting in the garbage queue.
    assert_eq!(
        garbage.drain(),
        1,
        "old song should be in the garbage queue"
    );
}
