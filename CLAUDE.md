# Live Set Player

An AbleSet-style setlist player that does its own audio — backing tracks, generated
click, TTS cues — with no Ableton Live or DAW underneath it. Rust daemon (audio + HTTP)
serving a Svelte frontend. Linux-first, minimal footprint.

**Full specification: `docs/SPEC.md`. Read it before starting any task.** The scope was
revised; SPEC.md §0 lists what is a permanent non-goal versus what is merely deferred,
and §13 is the current build order. `docs/PROMPTS.md` is a historical record of the
original phases 1–6 and does not describe current work.

**Migration in progress.** The shell is being moved from Tauri to a headless HTTP daemon
(SPEC.md §9.5). Until that lands, `src-tauri/` still holds a Tauri app; the engine at
`src-tauri/engine/` is unaffected and has never depended on Tauri.

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

7. **The frontend holds no authority.** It sends requests and renders status snapshots;
   the daemon owns audio, project state, and config. Never cache authoritative state in
   the UI or compute timing there from a stale sample count — it must stay correct when
   a second client connects (SPEC.md §9.5).

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
- Frontend: Svelte + TypeScript, built to a static bundle served by the daemon. No SSR,
  no Node in the run path. The UI never touches engine state directly; it calls `/api/*`
  and subscribes to the `/api/events` status stream.
- Prefer small, testable commits. Run the drift tests after any change to timing code.

## Verification

`cargo test --workspace` must pass (bare `cargo test` has the same `lsp-engine`-skipping
problem as bare `cargo clippy` above, for the same reason). Timing changes must also be checked with the offline
renderer (`docs/SPEC.md` §12), which renders a song to a stereo WAV with backtrack
left and click/cues right for inspection in a DAW.

## Out of scope

No plugin hosting. No waveform editor. No time-stretching or warping. No recording.
No Ableton Live integration, Link sync, or `.als` parsing — not needing them is the
point. Do not add these even if they seem helpful.

Separately **deferred, not forbidden** (SPEC.md §0): LAN remote UI, lyrics, OSC,
per-role performance layouts, Windows support. Don't build them yet; don't design
them out either.
