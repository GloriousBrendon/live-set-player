//! Phase-2 acceptance CLI: play a real backtrack WAV through an explicitly chosen
//! output device, with live section transport — the "done when" check for the
//! real-time engine (a backtrack audibly plays, a loopable section loops without a
//! seam, manual advance lands on the next bar boundary).
//!
//! Usage:
//!   play --list
//!   play --device "Speakers (Realtek...)" --wav track.wav --bpm 178 \
//!       [--time-sig 4/4] [--offset-samples 0] [--project-rate 48000] \
//!       [--sections "Intro:1:4,Verse:5:8,Chorus:13:8:loop,Bridge:21:4"] [--loop] \
//!       [--config lsp-config.json]
//!
//! With `--config`, the device name is persisted there (§1: by name, app config,
//! never in the project) and later runs may omit `--device`. With no configured
//! and no explicit device the player refuses to start — no default-device fallback.
//!
//! Interactive commands (stdin):
//!   a          advance to the next section (quantised to the next bar boundary)
//!   s <n>      seek to section n (playing: quantised jump; stopped: arm it)
//!   p          play        x  stop        !  panic stop
//!   g <db>     backtrack gain in dB       m  toggle backtrack mute
//!   q          quit

use lsp_engine::config::AppConfig;
use lsp_engine::device;
use lsp_engine::loader;
use lsp_engine::path::RelPath;
use lsp_engine::project::{
    AudioFileRef, BusLayout, ClickConfig, CueConfig, DownmixMode, Project, Section, Song, Track,
    TrackKind,
};
use lsp_engine::render::{AudioBank, CueBank};
use lsp_engine::rt::{self, Command, QueuedStatus, TransportState};
use lsp_engine::timeline::{Grid, TimeSignature};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

struct Args {
    list: bool,
    device: Option<String>,
    wav: Option<PathBuf>,
    bpm: f64,
    time_sig: TimeSignature,
    offset_samples: i64,
    project_rate: u32,
    sections: Option<Vec<Section>>,
    loop_single: bool,
    config: Option<PathBuf>,
    /// Unattended smoke-test mode: play for this many seconds, then exit (stdin
    /// EOF no longer quits — needed because some shells pre-drain a piped stdin).
    seconds: Option<u64>,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        list: false,
        device: None,
        wav: None,
        bpm: 120.0,
        time_sig: TimeSignature::FOUR_FOUR,
        offset_samples: 0,
        project_rate: 48000,
        sections: None,
        loop_single: false,
        config: None,
        seconds: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut next = || it.next().ok_or_else(|| format!("{flag} requires a value"));
        match flag.as_str() {
            "--list" => args.list = true,
            "--device" => args.device = Some(next()?),
            "--wav" => args.wav = Some(PathBuf::from(next()?)),
            "--bpm" => args.bpm = next()?.parse().map_err(|e| format!("--bpm: {e}"))?,
            "--time-sig" => {
                let s = next()?;
                let (n, d) = s
                    .split_once('/')
                    .ok_or_else(|| format!("--time-sig must look like 4/4, got '{s}'"))?;
                args.time_sig = TimeSignature {
                    numerator: n.parse().map_err(|e| format!("--time-sig: {e}"))?,
                    denominator: d.parse().map_err(|e| format!("--time-sig: {e}"))?,
                };
            }
            "--offset-samples" => {
                args.offset_samples = next()?
                    .parse()
                    .map_err(|e| format!("--offset-samples: {e}"))?
            }
            "--project-rate" => {
                args.project_rate = next()?
                    .parse()
                    .map_err(|e| format!("--project-rate: {e}"))?
            }
            "--sections" => {
                let spec = next()?;
                let mut sections = Vec::new();
                for part in spec.split(',') {
                    let fields: Vec<&str> = part.split(':').collect();
                    if fields.len() < 3 {
                        return Err(format!(
                            "--sections entry '{part}' must be Name:start_bar:length_bars[:loop]"
                        ));
                    }
                    sections.push(Section {
                        name: fields[0].to_string(),
                        start_bar: fields[1].parse().map_err(|e| format!("'{part}': {e}"))?,
                        length_bars: fields[2].parse().map_err(|e| format!("'{part}': {e}"))?,
                        loopable: fields.get(3).is_some_and(|f| *f == "loop"),
                        cue_text: None,
                        cue_lead_beats: 4,
                    });
                }
                args.sections = Some(sections);
            }
            "--loop" => args.loop_single = true,
            "--config" => args.config = Some(PathBuf::from(next()?)),
            "--seconds" => {
                args.seconds = Some(next()?.parse().map_err(|e| format!("--seconds: {e}"))?)
            }
            "--help" | "-h" => {
                eprintln!("see the doc comment at the top of examples/play.rs");
                std::process::exit(0);
            }
            other => return Err(format!("unknown flag '{other}'")),
        }
    }
    Ok(args)
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(2);
        }
    };

    if args.list {
        match device::list_output_devices() {
            Ok(names) => {
                println!("output devices:");
                for n in names {
                    println!("  {n}");
                }
            }
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
        return;
    }

    // Resolve the device name: explicit flag wins (and is persisted if --config was
    // given); otherwise the config supplies it; otherwise refuse (§1: no fallback).
    let mut config = match &args.config {
        Some(path) => match AppConfig::load(path) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("error loading config: {e}");
                std::process::exit(1);
            }
        },
        None => AppConfig::default(),
    };
    if let Some(name) = &args.device {
        config.output_device_name = Some(name.clone());
        if let Some(path) = &args.config {
            if let Err(e) = config.save(path) {
                eprintln!("warning: could not save config: {e}");
            }
        }
    }
    let device_name = match config.require_device() {
        Ok(n) => n.to_string(),
        Err(e) => {
            eprintln!("error: {e}");
            eprintln!("hint: run with --list to see devices, then pass --device \"<name>\"");
            std::process::exit(1);
        }
    };

    let Some(wav_path) = args.wav.clone() else {
        eprintln!("error: --wav is required (or use --list)");
        std::process::exit(2);
    };

    // Open the device first: the engine rate (and therefore what we resample the
    // project to) is only known once the device is open (§1).
    let (opened, mut handle, mut garbage) =
        match device::open_output(&device_name, args.project_rate, config.buffer_frames) {
            Ok(ok) => ok,
            Err(e) => {
                eprintln!("error opening device: {e}");
                std::process::exit(1);
            }
        };
    println!(
        "opened '{device_name}': engine rate {} Hz, {} channels",
        opened.engine_rate, opened.channels
    );
    if let Some(notice) = &opened.notice {
        println!("notice: {notice}");
    }

    // Load and prepare the song on a worker thread while the (silent) stream runs.
    println!("loading {} ...", wav_path.display());
    let audio = match loader::load_track_audio(&wav_path, DownmixMode::Sum, opened.engine_rate) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error loading WAV: {e}");
            std::process::exit(1);
        }
    };
    println!(
        "loaded: {} frames at {} Hz (mono)",
        audio.len(),
        opened.engine_rate
    );

    let grid = match Grid::new(args.project_rate, args.bpm, args.time_sig) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };
    let sections = args.sections.clone().unwrap_or_else(|| {
        // One section covering the whole file (at the project rate, which is the
        // source-frame coordinate system).
        let source_frames = if args.project_rate == opened.engine_rate {
            audio.len() as i64
        } else {
            (audio.len() as f64 * args.project_rate as f64 / opened.engine_rate as f64) as i64
        };
        let spb = grid.samples_per_pulse() * grid.pulses_per_bar() as f64;
        let bars = (((source_frames - args.offset_samples) as f64) / spb).floor() as u32;
        vec![Section {
            name: "Full".into(),
            start_bar: 1,
            length_bars: bars.max(1),
            loopable: args.loop_single,
            cue_text: None,
            cue_lead_beats: 4,
        }]
    });
    println!("sections:");
    for (i, s) in sections.iter().enumerate() {
        println!(
            "  [{i}] {} (source bar {}, {} bars{})",
            s.name,
            s.start_bar,
            s.length_bars,
            if s.loopable { ", loops" } else { "" }
        );
    }

    let project = Project {
        schema_version: lsp_engine::project::SCHEMA_VERSION,
        name: "play example".into(),
        sample_rate: args.project_rate,
        bus_layout: BusLayout::default(),
        click: ClickConfig::default(),
        cue: CueConfig::default(),
        songs: vec![],
    };
    let song = Song {
        id: "song".into(),
        title: wav_path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "song".into()),
        bpm: args.bpm,
        time_signature: args.time_sig,
        offset_samples: args.offset_samples,
        count_in_bars: 1,
        accent_pattern: vec![],
        sections,
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
    };

    let mut bank = AudioBank::new();
    bank.insert("bt", audio);
    let loaded =
        match rt::prepare_loaded(&project, &song, &bank, &CueBank::new(), opened.engine_rate) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("error preparing song: {e}");
                std::process::exit(1);
            }
        };
    handle
        .send(Command::LoadSong(loaded))
        .ok()
        .expect("command queue full at load");
    handle.send(Command::Play).ok().expect("command queue full");
    println!(
        "playing. commands: a=advance  s <n>=seek  p=play  x=stop  !=panic  g <db>  m=mute  q=quit"
    );

    // stdin reader thread; main loop owns the stream (cpal::Stream is !Send).
    let (line_tx, line_rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut line = String::new();
        loop {
            line.clear();
            match std::io::BufRead::read_line(&mut stdin.lock(), &mut line) {
                Ok(0) | Err(_) => return, // EOF or error: closing the channel quits the player
                Ok(_) => {}
            }
            if line_tx.send(line.trim().to_string()).is_err() {
                return;
            }
        }
    });

    let mut muted = false;
    let mut last_line = String::new();
    let started = std::time::Instant::now();
    loop {
        garbage.drain();

        if let Some(code) = opened.device_error() {
            eprintln!(
                "\nDEVICE ERROR ({}): output device lost or stream failed. \
                 Transport stopped; restart the player to re-open.",
                if code == 1 {
                    "device not available"
                } else {
                    "stream error"
                }
            );
            break;
        }

        if let Some(secs) = args.seconds {
            if started.elapsed() >= Duration::from_secs(secs) {
                println!("--seconds {secs} elapsed, exiting");
                break;
            }
        }

        if let Some(st) = handle.latest_status() {
            let state = match st.state {
                TransportState::Stopped => "stopped",
                TransportState::Playing => "playing",
                TransportState::Stopping => "stopping",
            };
            let queued = match st.queued {
                QueuedStatus::None => String::new(),
                QueuedStatus::Section(i) => format!("  -> queued: section {i}"),
                QueuedStatus::EndOfSong => "  -> queued: end".to_string(),
            };
            // Position rounded to whole seconds so the line reprints ~1/s while
            // playing, plus immediately on any state/section/queue change.
            let line = format!(
                "[{state}] section {}  bars left {}  pos {}s{}{}",
                st.section,
                st.bars_remaining,
                st.perf_sample / st.engine_rate as i64,
                queued,
                if st.mmcss_pro_audio {
                    "  (MMCSS Pro Audio)"
                } else {
                    ""
                },
            );
            if line != last_line {
                println!("{line}");
                last_line = line;
            }
        }

        match line_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(cmd) => {
                let mut parts = cmd.split_whitespace();
                match parts.next() {
                    Some("a") => drop(handle.send(Command::AdvanceSection)),
                    Some("s") => {
                        if let Some(Ok(n)) = parts.next().map(str::parse::<usize>) {
                            drop(handle.send(Command::SeekToSection(n)));
                        }
                    }
                    Some("p") => drop(handle.send(Command::Play)),
                    Some("x") => drop(handle.send(Command::Stop)),
                    Some("!") => drop(handle.send(Command::PanicStop)),
                    Some("g") => {
                        if let Some(Ok(db)) = parts.next().map(str::parse::<f32>) {
                            drop(handle.send(Command::SetTrackGainDb { track: 0, db }));
                        }
                    }
                    Some("m") => {
                        muted = !muted;
                        drop(handle.send(Command::SetTrackMuted { track: 0, muted }));
                    }
                    Some("q") => break,
                    _ => {}
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            // In --seconds mode a closed stdin doesn't end the run (some shells
            // pre-drain a piped stdin before the process even starts); the timer
            // owns the lifetime instead.
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                if args.seconds.is_none() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }

    drop(opened); // stops the stream
    garbage.drain();
    println!("bye");
}
