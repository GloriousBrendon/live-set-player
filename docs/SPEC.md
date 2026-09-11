# Live Set Player — Build Specification

Build an application for live band performance playback: backing tracks, generated
click, and spoken cues, driven from a setlist.

**Scope (revised).** The target is an **AbleSet-style setlist player that does its own
audio** — no Ableton Live, no DAW, no plugin host underneath it. AbleSet is a
*controller* that needs Ableton to make sound; this is the controller and the sound
engine in one process.

**Stack:** Rust daemon (audio + HTTP), Svelte frontend served over HTTP.
**Primary target:** Linux, native, minimal resources. Windows is a possible later
target, not a current one (§1, §14).

---

## 0. Design principles

Read these before implementing anything; they resolve most ambiguity below.

1. **It must not fail on stage.** Stability beats latency, beats features. There is no
   live input monitoring, so use large audio buffers (512–1024+ frames) and optimise
   purely for xrun immunity.
2. **Restructuring a song must never require touching audio or re-recording anything.**
   Sections are reorderable list entries; the click is generated; cues are synthesised.
   Rename a section and its spoken cue regenerates automatically.
3. **The audio thread is sacred.** No allocation, no locks, no I/O, no `Drop` of heap
   data inside the audio callback. Ever.
4. **Everything derives from one sample clock.** Click, cues, and section boundaries are
   all computed from absolute sample position. Nothing is incremented or accumulated.
5. **The UI is a network client, not a part of the process.** The daemon owns audio,
   project state, and config, and exposes them over a local HTTP API (§9.5). The
   frontend is a static web bundle that talks to that API and holds no authority. This
   is what keeps the resource footprint small *and* what makes the phase-2 LAN remote
   (§15) a bind-address change rather than a rewrite.

### Non-goals

Permanently out of scope. Do not add these even if they seem helpful.

- No plugin hosting.
- No arrangement/waveform timeline editor.
- No time-stretching or warping. Audio always plays at native speed.
- No audio recording.
- No Ableton Live integration, Link sync, or `.als` parsing. The point of the rewrite
  is not needing them.

### Deferred

Wanted eventually, deliberately not in the current build. Distinct from non-goals:
do not build these yet, and do not design them out either.

- **LAN remote UI** — band members controlling the set from phones/tablets (§15).
  The §9.5 daemon architecture exists so this is cheap later; it is not current scope.
- **Lyrics** — per-section lyric text and a scrolling lyric display.
- **OSC input/output.**
- **Per-role performance layouts** (drummer sees click/tempo, singer sees lyrics).
- **Windows support** (§1, §14).

---

## 1. Audio device and sample rate

**Device selection is explicit, never "default".** The default output device changes when
HDMI enumerates, when Bluetooth connects, or when the interface is plugged in after
launch. Silently falling back to laptop speakers mid-gig is the worst possible failure.

Requirements:

- Enumerate output devices via `cpal`; present a picker in settings.
- Persist the selected device **by name**, not by index, in app config (not the project
  file — the drummer's laptop has different hardware).
- On load, if the saved device is absent: refuse to enter playback state, show a loud,
  unmissable error in the performance view. Do not fall back to another device.
- Handle mid-session device loss without panicking the process: stop transport, surface
  the error, offer a re-open button.

**Sample rate:**

- The project declares a single `sample_rate` (44100 or 48000).
- **Linux (ALSA/PipeWire) — the supported platform:** open the device at exactly the
  project rate. If unsupported, fail loudly and say which rates the device does support.
- **Windows (WASAPI shared mode) — deferred, code retained.** The engine already handles
  this case: shared mode advertises only the mix format from the Sound control panel, so
  the project rate cannot be forced and Windows would resample underneath you silently.
  The handling is to read the device's actual rate at open time, treat it as the engine
  rate, resample the project to it at load, and surface the mismatch as an informational
  notice. **Keep this code and `mmcss.rs`** — they are written, tested, and harmless.
  Do not extend them, do not test against them, do not let them constrain Linux work.
- In both cases the engine has exactly one rate for its whole lifetime, fixed at device
  open. Nothing is resampled at playback time.
- **All audio is resampled to the project rate at load time**, offline, using `rubato`.
  A 44.1k file played on a 48k device without resampling runs 8.8% fast and sharp.
- All audio is also **downmixed to mono at load time** (sum L+R with −6 dB, configurable
  per track: `sum`, `left`, `right`). Every track lands on a mono bus anyway, so doing
  this once at load keeps the real-time path trivial.
- Post-load, every track in memory is `Arc<[f32]>`, mono, at project rate. Preload
  everything — audio and rendered cues — at set-load time.

---

## 2. Timeline model

This is the core of the application. Get it exactly right.

- Transport position is `u64` samples since song start.
- BPM counts **quarter notes per minute** (the DAW convention, not a 4/4-only
  shorthand). The click grid ticks in **pulses**, where one pulse is one tick of the
  time signature's *denominator* — a quarter note in 4/4, an eighth note in 7/8 — so
  the pulse duration also depends on the denominator:
  `samples_per_pulse: f64 = sample_rate * 60.0 / bpm * 4.0 / denominator`.
  In 4/4 (denominator 4) this reduces to the old `sample_rate * 60.0 / bpm`, so 178 BPM
  @ 48 kHz is still 16179.775 samples/pulse — **not an integer**. In 7/8 (denominator
  8) at 178 BPM it's 8089.887640449438 samples/pulse, and pulses tick at 356/minute,
  not 178/minute. A bar is `numerator` pulses.
- Convert bar/beat to samples from the **absolute pulse index**:
  `sample = round(absolute_pulse_index * samples_per_pulse) + song.offset_samples`
- **Never** compute the next pulse by adding `samples_per_pulse` to the previous one.
  Incremental accumulation drifts audibly within a few minutes.

**Performance time vs. source frames.** There are two coordinate systems, and keeping
them separate is what makes reordering a song free:

- **Performance time** is the output/transport timeline. Pulse 0 is the first beat of
  the first performance bar. Click, cues, and section boundaries are all scheduled in
  this space, and it has **no offset** — `sample = round(absolute_pulse_index *
  samples_per_pulse)`, full stop.
- **Source frames** are positions inside a backtrack file:
  `source_frame = round(source_pulse_index * samples_per_pulse) + song.offset_samples`.
  `offset_samples` (where bar 1 beat 1 sits inside the backtrack file — see below)
  applies **only** here, never to performance-time scheduling.

Resolving a performance order (§7) is the mapping from performance-time pulses to
source frames. The click scheduler never needs to know a reorder happened; it only
ever asks the grid for performance-time positions.

**Song offset:** every song has `offset_samples` — where bar 1 beat 1 sits inside the
backtrack file. Reaper renders frequently carry leading silence. Expose this as a nudge
control in the editor (± samples and ± ms). Without it, the click is mathematically
perfect and audibly wrong.

**Assumption:** one constant BPM and time signature per song, for the whole song. No
tempo map. State this as a known limitation in the README.

---

## 3. Threading and real-time discipline

Threads:

| Thread | Owns | Never does |
|---|---|---|
| Audio (cpal callback) | Transport, mixer, click synth, scheduler | Allocate, lock, block, drop heap data, log |
| HTTP / async runtime | API handlers, event stream, project state | Touch engine state directly; block on audio |
| Worker | File load, resample, Piper cue rendering, offline render | Block the API |

Communication:

- **API → audio:** lock-free SPSC queue (`rtrb`) carrying a `Command` enum
  (`Play`, `Stop`, `ArmSong`, `AdvanceSection`, `SetTrackGain`, `SetMute`, `LoadSet`, …).
- **Audio → API:** second lock-free queue carrying position, current section, bars
  and seconds remaining, and xrun counts. The API layer reads the latest snapshot and
  pushes it to clients at ~30 Hz (§9.5).
- **Simple continuous params** (gains) may use `AtomicU32` bit-cast floats instead.
- **Freeing memory:** when the audio thread releases an `Arc` to old audio data, push it
  to a garbage queue for the worker thread to drop. Deallocating in the callback causes
  xruns.

**Parameter smoothing:** every gain change and every mute/unmute ramps over 5–10 ms.
Un-ramped mutes click. This is not optional.

---

## 4. Bus routing

The live rig splits one stereo output: backtrack to FOH, click and cues to in-ears.

- Implement as **N mono buses mapped to output channels**, with the default config being
  2 buses → channels 0 and 1. Do not hardcode L/R. A 4-output interface later should be
  a config change, not a rewrite.
- Every track and generated element is assigned to a bus. Hard switch, not pan.
- Defaults: backtrack(s) → bus 0 (L). Click + cues → bus 1 (R).
- Per-track: gain (dB), mute, bus assignment. All live-adjustable.
- Sum sources per bus, then a safety limiter per bus (soft knee, ceiling −1 dBFS).
  **Default the limiter OFF on the click/cue bus** — it will duck the click every time a
  cue speaks. Make it a toggle with a tooltip explaining exactly that.
- Optional per-track 3-band EQ (biquad: low shelf / peaking / high shelf), coefficients
  computed outside the callback and passed in via the command queue.

---

## 5. Generated click

Real-time synthesised metronome. Not sample playback.

- Short enveloped tone: sine or triangle with fast exponential decay (~30–50 ms).
- Accent: distinct pitch and level on beat 1. Configurable accent pattern per song as an
  array of intensities, one per **pulse** in the bar (i.e. length equals the time
  signature's numerator) — e.g. `[2,0,0,0]` for 4/4, `[2,0,0,1,0,0,0]` for 7/8, where
  each entry corresponds to one eighth-note pulse, not one quarter-note beat. Supports
  odd meters.
- Independent click gain, separate from backtrack gain, on its own bus.
- The click scheduler reads beat positions from the timeline model in §2, so it cannot
  drift from section timing by construction.

---

## 6. Count-in

- Configurable count-in length in bars (default 1, range 0–4), per song, overridable
  globally.
- During count-in: click plays, backtrack is silent, transport position is negative
  relative to song start.
- Count-in also applies when starting from a section mid-song (rehearsal), using that
  section's downbeat as the target.
- Performance view shows a large count-in indicator with beats remaining.
- Count-in is **skipped** on an automatic song change (§9.2) unless the incoming song
  sets `count_in_bars > 0` and the setlist gap is long enough to contain it; see §9.2.

---

## 7. Sections and transport

Each song has BPM, time signature, and an **ordered list** of named sections:

```json
{
  "name": "Chorus",
  "start_bar": 17,
  "length_bars": 8,
  "loopable": true,
  "cue_text": null,
  "cue_lead_beats": 4
}
```

- `start_bar` refers to the position in the **source audio**; list order is the
  **performance order**. Reordering the list rearranges the song without touching audio.
- `loopable` sections repeat seamlessly until manually advanced.
- Manual advance quantises to the **next bar boundary**. The UI must show the queued
  next section between the trigger and the boundary.
- `cue_text: null` means "use the section name" — this is the default and the common case.

**Playhead jumps.** Looping back, or advancing to a non-adjacent section in a single
mixed backtrack, is an audio splice. Apply a **15 ms equal-power crossfade** at every
jump: keep the outgoing read position alive for the fade duration and mix both.

Document the known limitation: reverb tails, cymbal rings, and sustained vocals get cut
at splice points. Mitigation is exporting backtracks with section boundaries landing on
clean transients, or using stems (§10).

---

## 8. Spoken cues (TTS, offline, pre-rendered)

Retained in full under the revised scope. AbleSet has no equivalent; this is the feature
that makes restructuring a set a text edit.

Cues are **always TTS**. A robotic voice is acceptable and expected.

- Engine: **Piper**, via a prebuilt CLI sidecar invoked from the worker thread — not the
  `piper-rs` crate. The crate was tried first and rejected: it unconditionally vendors and
  compiles espeak-ng via `bindgen`, which requires `libclang` + CMake on *every* machine
  that runs `cargo build`, not just at release time. The sidecar CLI is instead built
  once, from a pinned `OHF-Voice/piper1-gpl` commit (GPL-3.0), in the release job only —
  see `src-tauri/binaries/README.md` for the pinned commit, build steps, and
  license-bundling details.
- Ship one voice model with the app; allow the user to point at additional `.onnx` +
  `.json` voice files in settings.
- Cues render **at edit time, on the worker thread**. Never in the audio thread, never
  at the gig.
- **Cache by content hash** of `(text, voice_id, speed)`. Store rendered WAVs in the
  project's `cues/` directory. Re-editing a set does not regenerate unchanged cues.
- **Renaming a section automatically triggers regeneration of its cue.** Do not require a
  manual "render cues" step, though provide one for bulk re-render.
- Cues are resampled and downmixed to project rate/mono like all other audio, then
  preloaded into memory at set-load.

**Scheduling — schedule by clip END, not start.** Cue clips vary in length ("Chorus" is
~0.4 s, "last time through the bridge" is ~2 s). Since clips are pre-rendered, their
duration is known:

```
cue_start_sample = section_downbeat_sample
                 - (cue_lead_beats * samples_per_beat)
                 - clip_length_samples
```

So `cue_lead_beats` means "the cue finishes speaking this many beats before the
downbeat" (default 4 — one bar of 4/4). If the resulting start would collide with a
previous cue or fall before the current position, start as early as possible and flag a
warning on that section in the editor.

**Known limitation — cues after a loopable section.** A section immediately following
a `loopable` one can't reliably get its full `cue_lead_beats` of lead time live: a
loopable section repeats an unknown number of times until a manual advance, so the
following section's downbeat isn't knowable far enough ahead, and the cue instead
starts as early as possible (from the moment the advance actually lands), the same
too-late fallback above, every time — not just occasionally. This is exposed as a field
(`follows_loopable_section`) on `lsp_engine::cue_schedule::ScheduledCue` rather than left
implicit here — the editor renders that field as a warning badge directly.

Cues route to the click/cue bus with independent gain.

**Sidecar resolution.** `lsp_engine::tts` takes already-resolved paths and has no opinion
about packaging. Under the daemon layout (§14) the sidecar binary and `resources/`
directory sit beside the daemon executable; resolution is `current_exe().parent()`.

---

## 9. Setlist, transport, and control

**Setlist view:** ordered songs, each expandable into its section list. Drag to reorder
songs and sections. Duplicate/disable a song without deleting it.

**Actions:** `arm next song`, `play`, `advance section`, `stop`, `panic stop` (immediate
silence, all buses).

### 9.1 Countdowns

The performance view is a clock as much as a label. Alongside `bars_remaining`, the
status snapshot (§3) carries **time**, derived on the audio thread from `perf_sample`
and the resolved performance order — never accumulated, never computed in the frontend
from a stale sample count:

- `section_seconds_remaining` — until the current section's boundary.
- `song_seconds_remaining` — until the end of the performance order.
- `song_position_seconds` / `song_duration_seconds` — for a progress bar.

Display as `M:SS`, counting down, at a size readable from three metres. During a
`loopable` section, `song_seconds_remaining` is undefined (the repeat count is not
knowable) — report it as `None` and have the UI show the section countdown alone rather
than a lie.

### 9.2 Auto-continue setlist

Per song: `auto_continue: bool` (**default false**). Project-level: `gap_seconds: f64`
(default 0) — silence between songs.

The default is off, not on. Design principle 1 says nothing surprising happens on
stage, and audio starting on its own when nobody asked is the surprise that matters
most; opting a song in is one checkbox in the editor. A project migrated from schema
v1 therefore behaves exactly as it did before the feature existed.

- On reaching the end of a song's performance order with `auto_continue` set, the
  daemon arms and plays the next **enabled** song after `gap_seconds`, instead of
  stopping. Today end-of-song stops the transport; this is the behavioural change.
- With `auto_continue` false, the transport stops at end of song and the next song is
  armed but not started — the current behaviour, now an explicit per-song choice.
- Count-in (§6) on an auto-continued song plays **inside** the gap when the gap is long
  enough to contain it, and is skipped otherwise. It never delays the downbeat past
  `gap_seconds`.
- The performance view shows the gap counting down with the incoming song's title.
- A `stop` or `panic_stop` during the gap cancels the chain, as does manually arming
  a song or pressing play. The gap is a cancellable scheduled transition, not a
  blocking sleep.
- **Chaining lives in the host, not the audio thread and not the frontend.** It loads
  the next song's audio, which is I/O (invariant 1); and a frontend timer stops firing
  when the webview is backgrounded or the screen sleeps. See `setlist_driver.rs`.
- A natural end and a human stop both reach `TransportState::Stopped`, and the
  `Stopping` ramp that distinguishes them is too brief (5–10 ms) to detect reliably by
  polling. Intent is therefore recorded explicitly: every human transport action bumps
  a chain epoch *before* its command reaches the engine, and a pending chain fires only
  if its captured epoch is still current. Missing a stop must never start a song.

### 9.3 Control input

- **Keyboard shortcuts** for every action, always available.
- **MIDI input** via `midir` for a USB footswitch, with **MIDI learn**: enter learn mode,
  press the pedal, capture whatever it sends (note on, CC, program change) and bind it.
  Cheap footswitches send wildly inconsistent messages; do not hardcode any mapping.
- Debounce MIDI triggers (~150 ms) — footswitches bounce.
- Note: many pedals present as USB HID keyboards rather than MIDI. Keyboard shortcut
  support covers those for free.
- MIDI is owned by the daemon, not the browser. The Web MIDI API is not used.

### 9.4 Performance view

The primary screen during a gig, and as important as the audio:

- Current song title.
- Current section name, very large.
- **Bars remaining** until the next section, counting down.
- **Time remaining** in section and song (§9.1), plus a song progress bar.
- Next section name (and queued-section indicator when an advance is pending).
- Count-in indicator; inter-song gap countdown (§9.2).
- Device status / error banner.

High contrast, readable from three metres under stage lighting. Optimise this view for a
glance, not for information density.

### 9.5 Daemon and HTTP API

The application is a single Rust binary that owns audio, MIDI, project state, and config,
and serves the frontend and its API over HTTP on `127.0.0.1` (port configurable,
persisted in app config).

- **Static bundle:** the built Svelte app is served from the daemon, embedded in the
  binary or read from a directory beside it. The frontend is a static SPA — no SSR, no
  Node runtime at any point.
- **`/api/*`:** a REST surface mirroring the existing command set one-for-one (transport,
  project, device, MIDI, cues). Commands are already thin wrappers over engine calls;
  this replaces the attribute layer, not the logic.
- **`/api/events`:** a Server-Sent Events stream pushing the §3 status snapshot at ~30 Hz.
  Replaces UI polling — one connection, server-paced, and it is the same mechanism the
  deferred LAN remote (§15) will use for multiple clients.
- **Binding:** `127.0.0.1` only, for now. Widening to `0.0.0.0` is deliberately *not*
  done until §15 brings authentication with it.
- **No native dialogs.** File and folder selection is a server-side browse endpoint
  (`GET /api/browse?path=`) returning directory listings, not a GTK file picker. This
  works identically for a local browser and a remote one, and removes a desktop
  toolkit dependency.
- **Config:** `$XDG_CONFIG_HOME/live-set-player/config.json` (falling back to
  `~/.config`). Device selection, MIDI bindings, port, and custom voices live here —
  never in project data.

**The frontend holds no authority.** It sends requests and renders snapshots. It must
remain correct if a second client connects and changes something — state lives in the
daemon, and every mutation is reflected back through `/api/events` or a refetch, not
assumed locally.

---

## 10. Songs and tracks

**Default case:** one mono-summed backtrack file per song. Sections are marked regions
within it. Click and cues are generated and overlaid independently.

**Optional case:** a song uses multiple stem files (drums, bass, synths, …) instead of a
single backtrack. Each stem gets its own gain, mute, bus, and EQ, and all follow the same
bar/section timeline. Stems must share identical length and offset; validate on load.

---

## 11. Project format

A **project folder**, not a bare JSON file. A JSON file with absolute paths breaks the
moment it lands on the drummer's laptop.

```
MySet.lsp/
  project.json
  audio/
    disaster-kind.wav
    ...
  cues/
    a3f9c1....wav      # named by content hash
```

- All paths in `project.json` are **relative to the project folder**.
- Store a hash and frame count per audio file; on load, verify and warn clearly if a file
  is missing or has changed (re-exported backtrack).
- `project.json` carries a schema version. Adding `auto_continue` (§9.2) and
  `gap_seconds` is a schema bump with a migration, not a silent `#[serde(default)]` —
  the migration path exists from day one and this is its first real use.
- Device selection is app config, not project data.

---

## 12. Offline render mode

A headless command that renders a song or the whole set — click, cues, backtrack, in
performance order — to a stereo WAV, bus 0 to left and bus 1 to right.

This is the only realistic way to verify click alignment and cue timing without booking a
gig. Import the result into Reaper against the source backtrack and check the click sits
on the grid.

Expose it as a CLI subcommand of the daemon binary (`live-set-player render …`) and as a
UI button that calls the equivalent API endpoint.

---

## 13. Build order (revised)

Phases 1–6 of the original build order are **complete**: timeline model, audio engine
core, click synth, count-in, Piper cues, editor/performance UI, and MIDI learn all exist
and are tested. The work below is what the scope change adds.

1. ~~**Re-spec.** This document and `CLAUDE.md`.~~ **Done.**
2. **Daemon shell.** New HTTP binary: static bundle + `/api/*` + `/api/events` (§9.5).
   Port the existing command handlers across unchanged; replace the native file dialog
   with `/api/browse`; replace resource-directory lookups with `current_exe()`-relative
   paths; move config to XDG. Remove the desktop-shell dependency.
3. **Frontend to HTTP.** Replace invoke calls with `fetch`; replace status polling with
   an SSE subscription. Build to a static bundle.
4. ~~**Countdowns** (§9.1).~~ **Done** — status snapshot extended, readouts and progress
   bar in the performance view, covered by `engine/tests/countdowns.rs`.
5. ~~**Auto-continue** (§9.2).~~ **Done** — v1→v2 schema migration, `setlist_driver.rs`,
   editor controls. Still outstanding: the gap countdown in the performance view.
6. **Packaging** (§14).

**Note on ordering.** Steps 4 and 5 were built against the existing Tauri shell rather
than waiting for step 2, to meet a performance date. They are shell-agnostic — the
countdowns are engine-side and the driver is a plain thread over `AppState` — so the
daemon migration ports them rather than redoing them.

Deferred work (§0) is not in this list by design.

---

## 14. Build and distribution

Linux-first, minimal footprint.

- Output is **one binary** plus its static assets, the Piper sidecar, and voice
  resources. No webview runtime, no Node, no desktop toolkit dependency in the run path.
- Launch opens the default browser at the local URL, or the user pins a
  `chromium --app=http://127.0.0.1:PORT` launcher. Ship a `.desktop` entry doing the
  latter.
- Distribute as a tarball and/or AppImage built on Linux CI. No cross-compilation.
- Ship the Piper voice model and `espeak-ng-data` alongside the binary; GPL-3.0 license
  bundling for the sidecar is unchanged (`src-tauri/binaries/README.md`).
- Windows packaging is deferred with Windows support (§0, §1).

---

## 15. Deferred: LAN remote

Recorded here so the current architecture stays compatible with it, **not** as current
work. When it happens it should be: bind `0.0.0.0`, add a shared-secret or pairing
auth, allow multiple concurrent SSE subscribers, and make every mutation idempotent and
broadcast. §9.5's "the frontend holds no authority" rule is what makes that possible
without reworking the UI.
