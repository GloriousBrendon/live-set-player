//! Offline-render CLI, per `docs/SPEC.md` §12 ("Expose it both as a CLI flag and a UI
//! button" -- this is the CLI half; the UI button comes with the frontend in a later
//! phase).
//!
//! Produces the click-only WAV used for the phase-1 acceptance check: import into a
//! DAW at the same BPM and confirm the click sits on the grid for the full duration.
//!
//! Usage:
//!   render_click --bpm 178 --sample-rate 48000 --time-sig 4/4 --minutes 10 \
//!       --out click-178.wav
//!
//! Optional flags:
//!   --lead-in N        Samples of silence before performance sample 0 (default 0).
//!                       Pass a song's offset_samples to align the render with an
//!                       untrimmed source backtrack at 0:00 in a DAW.
//!   --stub MODE         silent (default) | ramp | tones -- what the backtrack track
//!                       contains. `ramp`/`tones` are for eyeballing/listening to
//!                       section placement, not for the click-grid check.
//!   --order 2,1,2,3,0    Comma-separated section indices. When given, replaces the
//!                       single --minutes-long section with N generated 4-bar
//!                       sections (N = highest index + 1) played in the given order,
//!                       for exercising reordering/splices. --minutes is ignored in
//!                       this mode.

use lsp_engine::click::ClickSynthConfig; // used to size the default tail so the last click's decay isn't truncated
use lsp_engine::path::RelPath;
use lsp_engine::project::{
    AudioFileRef, Bus, BusLayout, ClickConfig, DownmixMode, Project, Section, Song, Track,
    TrackKind,
};
use lsp_engine::render::{self, AudioBank, RenderOptions};
use lsp_engine::sections::PerformanceEntry;
use lsp_engine::timeline::TimeSignature;
use std::path::PathBuf;

struct Args {
    bpm: f64,
    sample_rate: u32,
    time_sig: TimeSignature,
    minutes: f64,
    lead_in: u64,
    out: PathBuf,
    stub: StubMode,
    order: Option<Vec<usize>>,
}

#[derive(Clone, Copy)]
enum StubMode {
    Silent,
    Ramp,
    Tones,
}

fn print_usage() {
    eprintln!(
        "Usage: render_click --bpm F --sample-rate N --time-sig N/N --minutes F --out PATH \
         [--lead-in N] [--stub silent|ramp|tones] [--order 2,1,2,3,0]"
    );
}

fn parse_args() -> Result<Args, String> {
    let mut bpm = 178.0;
    let mut sample_rate = 48000u32;
    let mut time_sig = TimeSignature::FOUR_FOUR;
    let mut minutes = 10.0;
    let mut lead_in = 0u64;
    let mut out = PathBuf::from("click.wav");
    let mut stub = StubMode::Silent;
    let mut order: Option<Vec<usize>> = None;

    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let mut next = || {
            args.next()
                .ok_or_else(|| format!("{flag} requires a value"))
        };
        match flag.as_str() {
            "--bpm" => bpm = next()?.parse().map_err(|e| format!("--bpm: {e}"))?,
            "--sample-rate" => {
                sample_rate = next()?.parse().map_err(|e| format!("--sample-rate: {e}"))?
            }
            "--time-sig" => {
                let s = next()?;
                let (n, d) = s
                    .split_once('/')
                    .ok_or_else(|| format!("--time-sig must look like 4/4, got '{s}'"))?;
                time_sig = TimeSignature {
                    numerator: n
                        .parse()
                        .map_err(|e| format!("--time-sig numerator: {e}"))?,
                    denominator: d
                        .parse()
                        .map_err(|e| format!("--time-sig denominator: {e}"))?,
                };
            }
            "--minutes" => minutes = next()?.parse().map_err(|e| format!("--minutes: {e}"))?,
            "--lead-in" => lead_in = next()?.parse().map_err(|e| format!("--lead-in: {e}"))?,
            "--out" => out = PathBuf::from(next()?),
            "--stub" => {
                stub = match next()?.as_str() {
                    "silent" => StubMode::Silent,
                    "ramp" => StubMode::Ramp,
                    "tones" => StubMode::Tones,
                    other => {
                        return Err(format!("--stub must be silent|ramp|tones, got '{other}'"))
                    }
                }
            }
            "--order" => {
                let s = next()?;
                let parsed: Result<Vec<usize>, _> =
                    s.split(',').map(|t| t.trim().parse()).collect();
                order = Some(parsed.map_err(|e| format!("--order: {e}"))?);
            }
            "--help" | "-h" => {
                print_usage();
                std::process::exit(0);
            }
            other => return Err(format!("unknown flag '{other}'")),
        }
    }

    Ok(Args {
        bpm,
        sample_rate,
        time_sig,
        minutes,
        lead_in,
        out,
        stub,
        order,
    })
}

fn make_track(bus: usize) -> Track {
    Track {
        id: "bt".into(),
        name: "Backtrack".into(),
        file: AudioFileRef {
            path: RelPath::new("audio/stub.wav").unwrap(),
            sha256: None,
            frames: None,
        },
        gain_db: 0.0,
        muted: false,
        bus,
        downmix: DownmixMode::Sum,
        kind: TrackKind::Backtrack,
    }
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {e}");
            print_usage();
            std::process::exit(2);
        }
    };

    let project = Project {
        schema_version: lsp_engine::project::SCHEMA_VERSION,
        name: "render_click CLI".into(),
        sample_rate: args.sample_rate,
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
    };

    // Pulses per minute = bpm * denominator / 4 (see docs/SPEC.md §2: BPM counts
    // quarter notes/minute, so the pulse -- one tick of the denominator -- ticks
    // faster than bpm/minute whenever the denominator is finer than a quarter note).
    let pulses_per_minute = args.bpm * args.time_sig.denominator as f64 / 4.0;

    let (sections, order, source_len_estimate): (Vec<Section>, Vec<PerformanceEntry>, i64) =
        match &args.order {
            None => {
                let total_pulses = (args.minutes * pulses_per_minute).ceil() as i64;
                let length_bars =
                    (total_pulses as f64 / args.time_sig.numerator as f64).ceil() as u32;
                let sections = vec![Section {
                    name: "Full".into(),
                    start_bar: 1,
                    length_bars: length_bars.max(1),
                    loopable: false,
                    cue_text: None,
                    cue_lead_beats: 4,
                }];
                let order = vec![PerformanceEntry::once(0)];
                (sections, order, 0)
            }
            Some(indices) => {
                let n_sections = indices.iter().copied().max().map(|m| m + 1).unwrap_or(0);
                let bars_per_section = 4u32;
                let sections: Vec<Section> = (0..n_sections)
                    .map(|i| Section {
                        name: format!("Section{i}"),
                        start_bar: 1 + (i as u32) * bars_per_section,
                        length_bars: bars_per_section,
                        loopable: false,
                        cue_text: None,
                        cue_lead_beats: 4,
                    })
                    .collect();
                let order: Vec<PerformanceEntry> =
                    indices.iter().map(|&i| PerformanceEntry::once(i)).collect();
                (sections, order, 0)
            }
        };
    let _ = source_len_estimate;

    let song = Song {
        id: "song1".into(),
        title: "render_click".into(),
        bpm: args.bpm,
        time_signature: args.time_sig,
        offset_samples: 0,
        count_in_bars: 1,
        accent_pattern: vec![],
        sections,
        tracks: vec![make_track(0)],
        disabled: false,
    };

    if let Err(e) = project_and_song_sanity_check(&project, &song) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }

    // Backtrack length must cover the highest source frame any section reads from.
    // Compute generously: (max start_bar + length_bars) worth of bars, converted to
    // samples, plus headroom for the crossfade reading past a section's nominal end.
    let max_source_bar = song
        .sections
        .iter()
        .map(|s| s.start_bar + s.length_bars)
        .max()
        .unwrap_or(1) as i64;
    let grid = lsp_engine::timeline::Grid::new(args.sample_rate, args.bpm, args.time_sig)
        .expect("valid grid");
    let backtrack_frames = (grid.bar_to_pulse(max_source_bar) as f64 * grid.samples_per_pulse())
        .ceil() as usize
        + args.sample_rate as usize; // +1s headroom

    let mut bank = AudioBank::new();
    let stub_audio = match args.stub {
        StubMode::Silent => render::silent_stub(backtrack_frames),
        StubMode::Ramp => render::ramp_stub(backtrack_frames.min(render::RAMP_STUB_MAX_FRAMES)),
        StubMode::Tones => {
            let bar_frames =
                (grid.samples_per_pulse() * grid.pulses_per_bar() as f64).round() as i64;
            let freqs: Vec<f64> = (0..8).map(|i| 220.0 * 1.5f64.powi(i)).collect();
            render::tone_map_stub(
                backtrack_frames,
                args.sample_rate,
                bar_frames.max(1),
                &freqs,
            )
        }
    };
    bank.insert("bt", stub_audio);

    let opts = RenderOptions {
        lead_in_samples: args.lead_in,
        tail_samples: (ClickSynthConfig::default().decay_ms / 1000.0
            * 1.5
            * args.sample_rate as f64)
            .ceil() as u64,
    };

    let audio = match render::render_song(&project, &song, &order, &bank, &opts) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("render failed: {e}");
            std::process::exit(1);
        }
    };

    if let Err(e) = render::write_wav(&args.out, &audio) {
        eprintln!("failed to write {}: {e}", args.out.display());
        std::process::exit(1);
    }

    eprintln!(
        "wrote {} ({} frames, {} Hz, {} ch) covering {:.1} bar(s)",
        args.out.display(),
        audio.frame_count(),
        audio.sample_rate,
        audio.channels,
        song.sections.iter().map(|s| s.length_bars).sum::<u32>()
    );
}

fn project_and_song_sanity_check(project: &Project, song: &Song) -> Result<(), String> {
    if song.sections.is_empty() {
        return Err("no sections generated -- check --minutes/--order".to_string());
    }
    let bus_count = project.bus_layout.buses.len();
    if project.click.bus >= bus_count {
        return Err(format!(
            "click bus {} is out of range for {bus_count} configured bus(es)",
            project.click.bus
        ));
    }
    for track in &song.tracks {
        if track.bus >= bus_count {
            return Err(format!(
                "track '{}' bus {} is out of range for {bus_count} configured bus(es)",
                track.name, track.bus
            ));
        }
    }
    Ok(())
}
