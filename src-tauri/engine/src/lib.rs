//! Live Set Player audio engine: timeline model, project data model, and offline
//! renderer (`docs/SPEC.md` §2, §7, §11, §12).
//!
//! This crate has **no Tauri dependency** and never will (CLAUDE.md: "The audio
//! engine is a standalone library crate with no Tauri dependency, so it can be unit
//! tested and driven headlessly by the offline renderer"). It is driven either by
//! `cargo test`, by the [`render`] module's `examples/render_click.rs` CLI, or later
//! by a real-time engine crate and a Tauri command layer that both sit on top of it --
//! neither of which this crate knows about.
//!
//! # Non-negotiable invariants (CLAUDE.md)
//!
//! - **Never accumulate beat positions.** Every sample position is
//!   `round(absolute_pulse_index * samples_per_pulse) + offset`, computed fresh from
//!   an absolute index, never by repeatedly adding a non-integer step. See
//!   [`timeline`] for the full rationale and the drift tests that guard it.
//! - **One project sample rate.** Everything in this crate is parameterised by a
//!   single `sample_rate`; there is no per-track or per-call resampling here (the
//!   actual resampling of loaded audio to that rate is a later phase's job).
//! - **Paths are relative, forward-slash strings inside the project file**, converted
//!   to platform paths only at the filesystem boundary. See [`path::RelPath`].
//!
//! # Module map
//!
//! Phase 1 (timeline + offline render):
//!
//! - [`timeline`] -- the bar/pulse <-> sample grid ([`timeline::Grid`]) and the
//!   performance-time/source-frame split ([`timeline::SourceMap`]).
//! - [`sections`] -- resolving a performance order against a song's section list into
//!   exact sample positions ([`sections::resolve_order`]).
//! - [`path`] / [`project`] -- the `project.json` schema (§7, §11), including schema
//!   versioning and migration.
//! - [`click`] -- click synthesis (§5): the per-sample hit formula shared by the
//!   offline and real-time paths, plus the offline pulse renderer.
//! - [`crossfade`] -- the 15 ms equal-power splice fade (§7).
//! - [`render`] -- the offline renderer (§12); since phase 2 a thin driver over
//!   [`core`], so the offline render *is* the live signal path, headless.
//!
//! Phase 2 (real-time engine, §1/§3/§4/§7):
//!
//! - [`core`] -- the shared block-based playback core both paths run; block-size
//!   invariant and allocation-free after construction.
//! - [`smoother`] / [`limiter`] -- 5–10 ms parameter ramps (invariant 5) and the
//!   per-bus soft-knee safety limiter (§4).
//! - [`rt`] -- transport state machine, `rtrb` command/status/garbage queues, and
//!   the headless `process` the device callback (and the tests) drive.
//! - [`loader`] -- worker-thread load pipeline: WAV decode, mono downmix,
//!   `rubato` resample to the engine rate, preload as `Arc<[f32]>` (§1).
//! - [`device`] / [`config`] -- explicit cpal device selection persisted by name
//!   (§1, no default fallback), platform sample-rate policy, stream wiring.
//! - [`mmcss`] -- Windows "Pro Audio" MMCSS registration for the callback thread
//!   (cpal does not do this; see the module docs for what was verified).
//! - [`midi`] -- MIDI learn-mode binding, debounce, and `midir` device enumeration
//!   (§9), persisted alongside the output device in [`config::AppConfig`].

pub mod click;
pub mod config;
pub mod core;
pub mod crossfade;
pub mod cue_schedule;
pub mod device;
pub mod error;
pub mod limiter;
pub mod loader;
pub mod midi;
pub mod mmcss;
pub mod path;
pub mod project;
pub mod render;
pub mod rt;
pub mod sections;
pub mod smoother;
pub mod timeline;
pub mod tts;
