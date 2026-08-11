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
//! - [`timeline`] -- the bar/pulse <-> sample grid ([`timeline::Grid`]) and the
//!   performance-time/source-frame split ([`timeline::SourceMap`]).
//! - [`sections`] -- resolving a performance order against a song's section list into
//!   exact sample positions ([`sections::resolve_order`]).
//! - [`path`] / [`project`] -- the `project.json` schema (§7, §11), including schema
//!   versioning and migration.
//! - [`click`] -- provisional click synthesis, real-time-safe in spirit but not yet
//!   wired into a real-time callback (that's phase 3's job).
//! - [`crossfade`] -- the 15 ms equal-power splice fade (§7).
//! - [`render`] -- the offline renderer (§12), the test harness for everything else.

pub mod click;
pub mod crossfade;
pub mod error;
pub mod path;
pub mod project;
pub mod render;
pub mod sections;
pub mod timeline;
