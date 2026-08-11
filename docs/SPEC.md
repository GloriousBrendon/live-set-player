# Live Set Player — Build Specification

Build a desktop application for live band performance playback. It replaces Ableton Live
for backing tracks, click, and spoken cues during gigs.

**Stack:** Tauri 2.x, Rust backend, Svelte frontend.
**Targets:** Linux and Windows (guest musicians bring their own laptops).

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

### Non-goals

- No plugin hosting.
- No arrangement/waveform timeline editor.
- No time-stretching or warping. Audio always plays at native speed.
- No audio recording.

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
- **Linux (ALSA/PipeWire):** open the device at exactly the project rate. If unsupported,
  fail loudly and say which rates the device does support.
- **Windows (WASAPI shared mode):** `cpal` uses shared mode, where the device advertises
  only the mix format configured in the Sound control panel. Do not attempt to force the
  project rate — you cannot, and Windows will resample underneath you without telling you.
  Instead: read the device's actual rate at open time, treat it as the engine rate, and
  resample the entire project to it at load time. Surface the mismatch in the UI as an
  informational notice, not an error.
- In both cases the engine has exactly one rate for its whole lifetime, fixed at device
  open. Nothing is resampled at playback time. Verify early which mode you are in — this
  is the first thing to check if timing behaves differently across platforms.
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

Three threads:

| Thread | Owns | Never does |
|---|---|---|
| Audio (cpal callback) | Transport, mixer, click synth, scheduler | Allocate, lock, block, drop heap data, log |
| UI (Tauri/Svelte) | Editing, project state, rendering | Touch engine state directly |
| Worker | File load, resample, Piper cue rendering, offline render | Block the UI |

Communication:

- **UI → audio:** lock-free SPSC queue (`rtrb`) carrying a `Command` enum
  (`Play`, `Stop`, `ArmSong`, `AdvanceSection`, `SetTrackGain`, `SetMute`, `LoadSet`, …).
- **Audio → UI:** second lock-free queue carrying position, current section, bars
  remaining, and xrun counts. UI polls at ~30 Hz.
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
  computed on the UI thread and passed in via the command queue, never computed in the
  callback.

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

Cues are **always TTS**. A robotic voice is acceptable and expected — the entire point is
that restructuring a set is a text edit, never a re-recording session.

- Engine: **Piper**, via the `piper-rs` crate. Do **not** shell out to a Python or CLI
  sidecar; bundling that into a cross-platform Tauri app is unnecessary pain. Note that
  upstream Piper is now `OHF-Voice/piper1-gpl` and is GPL-3.0 licensed.
- Ship one voice model with the app; allow the user to point at additional `.onnx` +
  `.json` voice files in settings.
- Cues render **at edit time, on the worker thread**. Never in the audio thread, never
  at the gig.
- **Cache by content hash** of `(text, voice_id, speed)`. Store rendered WAVs in the
  project's `cues/` directory. Re-editing a set does not regenerate unchanged cues.
- **Renaming a section automatically triggers regeneration of its cue.** This is the
  feature that makes restructuring cheap — do not require a manual "render cues" step,
  though provide one for bulk re-render.
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

Cues route to the click/cue bus with independent gain.

---

## 9. Setlist and control

**Setlist view:** ordered songs, each expandable into its section list. Drag to reorder
songs and sections. Duplicate/disable a song without deleting it.

**Actions:** `arm next song`, `play`, `advance section`, `stop`, `panic stop` (immediate
silence, all buses).

**Control input:**

- **Keyboard shortcuts** for every action, always available.
- **MIDI input** via `midir` for a USB footswitch, with **MIDI learn**: enter learn mode,
  press the pedal, capture whatever it sends (note on, CC, program change) and bind it.
  Cheap footswitches send wildly inconsistent messages; do not hardcode any mapping.
- Debounce MIDI triggers (~150 ms) — footswitches bounce.
- Identical behaviour on Linux and Windows.
- Note: many pedals present as USB HID keyboards rather than MIDI. Keyboard shortcut
  support covers those for free.

**Performance view** — the primary screen during a gig, and as important as the audio:

- Current song title.
- Current section name, very large.
- **Bars remaining until the next section**, counting down.
- Next section name (and queued-section indicator when an advance is pending).
- Count-in indicator.
- Device status / error banner.

High contrast, readable from three metres under stage lighting. Optimise this view for a
glance, not for information density.

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
- `project.json` carries a schema version. Write a migration path from day one.
- Device selection is app config, not project data.

---

## 12. Offline render mode

A headless command that renders a song or the whole set — click, cues, backtrack, in
performance order — to a stereo WAV, bus 0 to left and bus 1 to right.

This is the only realistic way to verify click alignment and cue timing without booking a
gig. Import the result into Reaper against the source backtrack and check the click sits
on the grid. Build this early; it is the test harness for everything else.

Expose it both as a CLI flag and a UI button.

---

## 13. Build order

1. **Test harness + timeline model.** Timeline maths (§2) with unit tests asserting no
   drift over 10+ minutes at non-integer BPMs. Offline render (§12).
2. **Audio engine core.** Device selection and sample-rate handling (§1), single-file
   backtrack playback, bar-accurate section markers, seamless looping with crossfade (§7),
   one shared clock, real-time discipline (§3).
3. **Click synth** (§5) locked to the same clock, plus count-in (§6).
4. **Piper cue rendering** (§8) — cache, auto-regenerate on rename, end-anchored
   scheduling.
5. **Minimal UI.** Load song, define sections by bar count, set loop flags, hit play.
   Performance view (§9).
6. **MIDI learn + footswitch + full setlist UI** (§9).
7. **Stems and EQ** (§10, §4) — additive, only once 1–6 are solid.

---

## 14. Build and distribution

- Build each platform on its own runner. **Do not attempt to cross-compile Windows from
  Linux** — Tauri's own documentation calls this a last resort that is not well tested.
- Use a GitHub Actions matrix (`ubuntu-22.04`, `windows-latest`) with `tauri-action`.
- Linux: AppImage and `.deb`. Windows: NSIS `.exe`.
- Ship the Piper voice model as a bundled resource.
