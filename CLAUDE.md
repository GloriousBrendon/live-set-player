# Live Set Player

Desktop app for live band performance playback (backing tracks, generated click,
TTS cues). Tauri 2 + Rust + Svelte. Targets Linux and Windows.

**Full specification: `docs/SPEC.md`. Read it before starting any task.**

## Invariants

These are non-negotiable. Violating any of them is a bug even if the code compiles
and sounds fine on your machine.

1. **The audio callback never allocates, locks, blocks, does I/O, logs, or drops
   heap data.** UI → audio communication goes over a lock-free SPSC queue (`rtrb`)
   or atomics. Buffers released by the audio thread are sent to a garbage queue and
   dropped on a worker thread.

2. **Never accumulate beat positions.** BPM is quarter-notes-per-minute (DAW
   convention); the click grid ticks in *pulses* (one tick of the time signature's
   denominator), so `samples_per_pulse = sample_rate * 60 / bpm * 4 / denominator` is
   an `f64` and is almost never an integer (178 BPM @ 48 kHz = 16179.775 in 4/4;
   8089.887640449438 in 7/8). Every event position is computed as
   `round(absolute_pulse_index * samples_per_pulse) + song.offset_samples`.
   Incremental addition drifts and is the single worst failure mode in this project.
   `offset_samples` applies only when mapping to source-file frames, never to
   performance-time scheduling — see `docs/SPEC.md` §2.

3. **One project sample rate.** All audio is resampled to it and downmixed to mono
   at load time, on a worker thread. Nothing is resampled at playback time.

4. **The output device is chosen explicitly and persisted by name.** Never fall back
   to a default device. If the saved device is missing, refuse to play and say so.

5. **Every gain or mute change ramps over 5–10 ms.** Un-ramped changes click.

6. **Every playhead jump gets a 15 ms equal-power crossfade.** Loops and section
   advances are audio splices.

## Priorities

Stability > correctness of timing > features > latency. There is no live input
monitoring, so prefer large audio buffers (512–1024+ frames) and optimise for xrun
immunity, not low latency.

## Conventions

- Rust: `cargo fmt`, `cargo clippy --workspace -- -D warnings` must pass before any commit.
  `--workspace` is required: this workspace has a root package (`lsp-scaffold`) alongside
  the `engine` member, so a bare `cargo clippy` from `src-tauri/` silently checks only
  `lsp-scaffold` and skips `lsp-engine` — where all the real logic lives — entirely.
- The audio engine is a standalone library crate at `src-tauri/engine/` (package
  `lsp-engine`, a workspace member of `src-tauri/`), with no Tauri dependency, so it
  can be unit tested and driven headlessly by the offline renderer.
- Frontend: Svelte + TypeScript. The UI never touches engine state directly; it
  sends commands and reads a status snapshot.
- Prefer small, testable commits. Run the drift tests after any change to timing code.

## Verification

`cargo test --workspace` must pass (bare `cargo test` has the same `lsp-engine`-skipping
problem as bare `cargo clippy` above, for the same reason). Timing changes must also be checked with the offline
renderer (`docs/SPEC.md` §12), which renders a song to a stereo WAV with backtrack
left and click/cues right for inspection in a DAW.

## Out of scope

No plugin hosting. No waveform editor. No time-stretching or warping. No recording.
Do not add these even if they seem helpful.
