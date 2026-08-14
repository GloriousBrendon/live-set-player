# Claude Code Session Prompts — Live Set Player

One session per phase. Do not run two phases in one session — long sessions compact,
and compaction is where architectural decisions get quietly forgotten.

## How to use this

- Save as `docs/PROMPTS.md` in the repo.
- Start each session with the launch command given for that phase.
- Paste the prompt verbatim.
- Every prompt ends with "plan first and wait for approval." Read the plan. This is the
  cheapest place to catch a misunderstanding.
- At the end of a phase: `cargo fmt`, `cargo clippy --workspace -- -D warnings`,
  `cargo test --workspace`, commit, then exit. Start the next phase in a fresh session.
  `--workspace` is required on clippy and test — this workspace has a root package
  (`lsp-scaffold`) alongside the `engine` member, so the bare commands silently skip
  `lsp-engine` and check/test only the empty scaffold app. `cargo fmt` is unaffected;
  it covers the whole workspace by default.
- Switching models mid-session re-reads the whole conversation uncached, so pick the model
  at launch rather than switching partway through.

**Development platform: Windows.** Linux is a supported target but is not being built or
tested during initial development. Every phase must stay portable; phase 8 sets up CI so
Linux builds are verified without a Linux machine.

---

## Phase 0 — Setup

Do all of this before opening Claude Code. Run it in PowerShell.

### 0.1 Toolchain

```powershell
winget install --id Rustlang.Rustup -e
winget install --id OpenJS.NodeJS.LTS -e
winget install --id Git.Git -e
winget install --id Microsoft.EdgeWebView2Runtime -e
winget install --id Microsoft.VisualStudio.2022.BuildTools -e `
  --override "--wait --passive --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"
```

Close and reopen the terminal, then:

```powershell
rustup default stable-msvc
rustc --version
node --version
```

Both must print a version. If `rustc` isn't found, the terminal didn't pick up the new
PATH — open a fresh one.

### 0.2 Defender exclusion (admin PowerShell, optional but worth it)

```powershell
Add-MpPreference -ExclusionPath "$HOME\dev\live-set-player\src-tauri\target"
```

Real-time scanning on Rust build output makes compiles several times slower.

### 0.3 Scaffold

```powershell
mkdir -Force $HOME\dev; cd $HOME\dev
npm create tauri-app@latest
```

Answer: name `live-set-player`, frontend language **TypeScript / JavaScript**, framework
**Svelte**, flavour **TypeScript**.

```powershell
cd live-set-player
npm install
npm run tauri dev
```

**A window must open.** Close it with Ctrl+C. Do not continue until this works — debugging
a WebView2 problem and an audio problem at the same time is miserable.

### 0.4 Rust dependencies

Do **not** add engine dependencies (`cpal`, `midir`, `rtrb`, `hound`, `rubato`,
`thiserror`, ...) to `src-tauri/Cargo.toml` (the Tauri app crate) here or at any point.
They belong to `src-tauri/engine/` (package `lsp-engine`), the standalone,
Tauri-free crate phase 1 creates as a workspace member — see CLAUDE.md's Conventions
section. Each phase's session adds the dependencies it needs to whichever crate it's
extending (phase 1: `serde`, `serde_json`, `hound`, `thiserror` in `engine/Cargo.toml`;
phase 2 onward: `cpal`, `rtrb`, `rubato`, etc., also in `engine/Cargo.toml` unless the
dependency is Tauri-command-layer-only). There is nothing to run manually in this step.

`piper-rs` gets added in phase 4, not now.

### 0.5 Documents

```powershell
mkdir docs
```

Place the three files:

- `CLAUDE.md` → repo root
- `live-set-player-spec.md` → `docs\SPEC.md` (rename it)
- this file → `docs\PROMPTS.md`

### 0.6 Commit and configure

```powershell
git init
git add -A
git commit -m "scaffold + spec"

[Environment]::SetEnvironmentVariable("CLAUDE_CODE_SUBAGENT_MODEL", "sonnet", "User")
```

Reopen the terminal so the environment variable takes effect. That one keeps subagent
fan-out — file reading, test running — off the expensive model.

### 0.7 Launch

```powershell
cd $HOME\dev\live-set-player
claude update
claude --model opusplan --effort xhigh
```

Inside the session, run `/model` once to see whether Fable is listed for your account.
If it is, save it for phase 2. Then paste the phase 1 prompt below.

---

## Phase 1 — Timeline model and offline render harness

You are already in this session from step 0.7. Paste:
> Read CLAUDE.md and docs/SPEC.md in full.
>
> This session covers phase 1 only: the timeline model and the offline render harness. Do not touch cpal, Tauri, or the frontend. Build `src-tauri/engine/` as a standalone library crate (its own `Cargo.toml`, a workspace member of `src-tauri/`) with no Tauri dependency, so it can be unit tested and driven headlessly.
>
> Deliverables:
>
> 1. The timeline model from §2 — bar/beat ↔ sample conversion, section boundary resolution, per-song `offset_samples`. Every position is computed as `round(absolute_beat_index * samples_per_beat) + offset`, never by accumulation.
> 2. The project data model from §7 and §11 — serde types for project, song, section, and track. Paths are stored as relative, forward-slash strings and converted to platform paths only at the filesystem boundary. Include the schema version field.
> 3. Unit tests asserting zero drift over 10+ minutes at 178, 143.5, and 91 BPM, at both 44100 and 48000 Hz. Include at least one test that fails under naive incremental accumulation and passes with the correct implementation — I want the regression guard to be real, not decorative.
> 4. Tests for section ordering: resolving a performance order that differs from source bar order, and computing absolute sample positions for each entry in that order.
> 5. The offline renderer from §12 as a plain function: given a project and a section order, produce an interleaved stereo f32 buffer with bus 0 on left and bus 1 on right, and write it to a WAV with hound. A silent backtrack stub is correct at this stage — a click-only render is the expected output.
>
> Plan first, show me the plan, and wait for approval before writing code.

**Done when:** `cargo test --workspace` passes, and a click-only WAV rendered at 178 BPM lines up with a 178 BPM grid in a DAW for ten straight minutes.

---

## Phase 2 — Audio engine core

```powershell
claude --model fable        # or: claude --model opus --effort xhigh
```

The hardest phase and the one worth the better model. Cross-cutting, ambiguous, and the
place where a subtle mistake costs you a gig.

> Read CLAUDE.md and docs/SPEC.md in full, then read the existing `src-tauri/engine/` module.
>
> Phase 1 is complete: the timeline model, project types, and offline renderer exist and are tested. This session builds the real-time audio engine on top of them. No Tauri commands and no frontend work yet — expose a clean Rust API that the offline renderer and a future Tauri layer can both drive.
>
> Scope: §1, §3, §4, and the playback and crossfade parts of §7.
>
> Requirements:
>
> - Device enumeration and explicit selection via cpal, persisted by device name in app config (separate from the project file). No fallback to a default device. Missing saved device means refusing to play, with a clear error surfaced through the status channel.
> - Sample rate per §1. I am developing on Windows, where cpal uses WASAPI shared mode and the device advertises only the mix format from the Sound control panel. Read the device's actual rate at open time, treat it as the engine rate, and resample the project to it at load time. The Linux path (open at the project rate, fail loudly if unsupported) must still be implemented and correct even though I cannot test it right now.
> - Load-time pipeline on a worker thread: decode WAV, resample with rubato to the engine rate, downmix to mono per §1, preload as `Arc<[f32]>`.
> - The mixer from §4: N mono buses mapped to output channels, defaulting to two buses on channels 0 and 1. Per-track gain, mute, and bus assignment. Per-bus soft limiter, defaulting to off on the click/cue bus.
> - Real-time discipline per §3 and CLAUDE.md: an `rtrb` command queue for UI→audio, a status queue for audio→UI, atomics for continuous gains, and a garbage queue so buffers are dropped on the worker thread. Gain and mute changes ramp over 5–10 ms.
> - Transport: play, stop, panic stop, seek to section, seamless looping of loopable sections, manual advance quantised to the next bar boundary with a queued-section state readable from the status channel.
> - A 15 ms equal-power crossfade on every playhead jump — loop wrap and section advance both.
> - Verify whether cpal's WASAPI backend registers the audio thread with MMCSS as a Pro Audio task. If it does not, register it, and tell me what you found.
>
> Constraints: no unsafe code in the audio path without justifying it to me first. Buffer sizes 512–1024 frames; do not optimise for latency. The offline renderer must continue to work and must produce identical sample output to the real-time path given the same input — add a test that asserts this.
>
> Plan first, show me the plan, and wait for approval before writing code.

**Done when:** a real backtrack plays through a chosen device, loops a section without an audible seam, and the offline render is sample-identical to the live path.

---

## Phase 3 — Click synth and count-in

```powershell
claude --model sonnet
```

> Read CLAUDE.md and docs/SPEC.md §5 and §6, then read `src-tauri/engine/`.
>
> Add the generated click synth and count-in to the existing engine.
>
> - Real-time synthesised click: enveloped tone with fast exponential decay, roughly 30–50 ms. Not sample playback.
> - Configurable per-beat accent pattern as an array of intensities, one entry per beat in the bar, supporting odd meters. Accent changes pitch and level.
> - Independent click gain on its own bus, separate from backtrack gain.
> - Count-in per §6: configurable 0–4 bars, click only with the backtrack silent, negative transport position relative to song start, and count-in applies when starting from a mid-song section too. Expose beats-remaining through the status channel.
> - The click scheduler reads beat positions from the phase 1 timeline model. It must not compute its own.
>
> Add tests that render click-only output through the offline renderer and assert transient positions match expected sample positions exactly, including for a 7/8 accent pattern and for a count-in.
>
> Plan first, show me the plan, and wait for approval before writing code.

**Done when:** the offline render puts every click transient on its exact expected sample, in 4/4 and 7/8, with and without count-in.

---

## Phase 4 — TTS cues

```powershell
claude --model sonnet
```

Start with the spike. If `piper-rs` fights you, stop and tell me before building around it.

> Read CLAUDE.md and docs/SPEC.md §8, then read `src-tauri/engine/`.
>
> Add offline TTS cue generation and scheduling.
>
> Start with a spike, before any integration work: add the `piper-rs` crate, load a voice model, and render a single WAV from the string "chorus" on Windows. Report back what the voice model loading story looks like — file layout, size, and how awkward it will be to bundle as a Tauri resource. If it does not work cleanly, stop and tell me rather than working around it; the fallback is a Piper sidecar binary via Tauri's externalBin and I want to make that call deliberately.
>
> Once the spike works:
>
> - Cue rendering runs on the worker thread at edit time. Never in the audio thread, never at playback.
> - Cache by content hash of (text, voice id, speed). Rendered WAVs live in the project's `cues/` directory, named by hash. Unchanged cues are never regenerated.
> - Renaming a section automatically triggers regeneration of its cue. Also provide an explicit bulk re-render. `cue_text: null` means use the section name.
> - Cues are resampled and downmixed like all other audio, preloaded at set load.
> - Schedule by clip end, not start, per the formula in §8: the cue finishes speaking `cue_lead_beats` before the section downbeat. If the computed start would fall before the current position or collide with a previous cue, start as early as possible and flag a warning on that section.
> - Cues route to the click/cue bus with independent gain.
>
> Add tests asserting the end-anchored scheduling maths for cue clips of different lengths, including the collision and too-late cases.
>
> Plan first, show me the plan, and wait for approval before writing code.

**Done when:** renaming a section regenerates its spoken cue with no manual step, and a long cue and a short cue both finish at the same point relative to the downbeat.

---

## Phase 5 — Minimal UI and performance view

```powershell
claude --model sonnet
```

> Read CLAUDE.md and docs/SPEC.md §7, §9, and §11, then read `src-tauri/engine/`.
>
> Build the Tauri command layer and a Svelte frontend. The UI never touches engine state directly: it sends commands over the existing command queue and polls a status snapshot at about 30 Hz.
>
> Two views:
>
> 1. **Editor** — load a project folder, define sections by start bar and length in bars, set loop flags and cue lead, reorder sections by drag, set per-track gain/mute/bus, set BPM, time signature, count-in bars, and the song offset with both sample and millisecond nudge controls. Not a waveform editor.
> 2. **Performance view** — the primary gig screen. Current song title, current section name very large, bars remaining until the next section counting down, next section name, queued-section indicator when an advance is pending, count-in indicator, and a device status banner. High contrast, readable from three metres under stage lighting. Optimise for a glance, not information density.
>
> Also implement project folder load and save per §11: relative forward-slash paths, hash and frame count verification per audio file with a clear warning on mismatch, and the schema version field. Test that a project folder saved on Windows loads correctly when paths are read back — do not let backslashes leak into the JSON.
>
> Keyboard shortcuts for every transport action: play, stop, panic stop, advance section, arm next song.
>
>Also: phase 4 left one thing unverified — whether Tauri's packaged resource layout places the ONNX Runtime shared libraries where the OS loader finds them relative to the sidecar binary inside a real bundle. Now that a frontend exists, run an actual tauri build and invoke the bundled sidecar from inside the packaged app to confirm it resolves. If it doesn't, fix the resource layout.
>
> Plan first, show me the plan, and wait for approval before writing code.

**Done when:** you can load a project, define sections, hit play, and watch the bar countdown advance correctly against what you hear.

---

## Phase 6 — MIDI footswitch and setlist

```powershell
claude --model sonnet
```

> Read CLAUDE.md and docs/SPEC.md §9, then read the existing engine and frontend.
>
> Add MIDI control input and the full setlist UI.
>
> - MIDI input via midir with a learn mode: enter learn, press the pedal, capture whatever it sends (note on, CC, or program change) and bind it to an action. Do not hardcode any mapping — cheap footswitches are wildly inconsistent.
> - Debounce triggers at roughly 150 ms.
> - Persist bindings in app config, by port name, alongside the audio device selection.
> - I am developing on Windows, where midir uses WinMM. WinMM truncates MIDI device names to 31 characters, so match persisted port names accordingly rather than assuming a full name round-trips. Windows also has no built-in virtual MIDI ports — tell me what I need to install to test this without hardware, and make the binding logic testable without a real device.
> - Setlist view: ordered songs, each expandable into its section list. Drag to reorder songs and sections. Duplicate or disable a song without deleting it.
> - Wire the arm-next-song, play, advance-section, stop, and panic-stop actions to both MIDI bindings and keyboard shortcuts through the same code path.
>
> Plan first, show me the plan, and wait for approval before writing code.

**Done when:** a footswitch advances sections identically to the keyboard shortcut, and rebinding a different pedal takes under a minute.

---

## Phase 7 — Stems and EQ

```powershell
claude --model sonnet
```

Optional and additive. Do not start this until phases 1–6 are solid and you have played a real set with the tool.

> Read CLAUDE.md and docs/SPEC.md §10 and the EQ portion of §4, then read the existing engine.
>
> Add multi-stem songs and per-track EQ.
>
> - A song may use multiple stem files instead of a single backtrack. Each stem gets its own gain, mute, bus, and EQ, and all follow the same bar/section timeline. Validate on load that stems share identical length and offset, with a clear error if not.
> - Per-track 3-band EQ: low shelf, peaking, high shelf, biquad-based. Coefficients are computed on the UI thread and passed in over the command queue. Never computed in the audio callback.
> - Extend the editor UI with a per-track panel.
>
> The single-backtrack path must remain the default and must not regress. Add a test covering a stem song and a backtrack song rendering through the same timeline.
>
> Plan first, show me the plan, and wait for approval before writing code.

---

## Phase 8 — Cross-platform CI

```powershell
claude --model sonnet
```

Run this once phase 2 is done — you want Linux breakage caught early, not at the end.

> Read docs/SPEC.md §14.
>
> Set up GitHub Actions to build and test on both platforms. I develop on Windows and cannot currently test on Linux, so CI is my only Linux verification.
>
> - Matrix over `windows-latest` and `ubuntu-22.04`.
> - On every push: `cargo fmt --check`, `cargo clippy --workspace -- -D warnings`, `cargo test --workspace` on both platforms. `--workspace` matters: this workspace has a root package (`lsp-scaffold`) alongside the `engine` member, so the bare commands silently skip `lsp-engine` and only check/test the empty scaffold app.
> - On tag push: `tauri-action` producing NSIS `.exe` for Windows and AppImage plus `.deb` for Linux, attached to a GitHub release.
> - Install the Linux system dependencies the runner needs, including the ALSA development headers that cpal and midir require — these are easy to forget because they are not in Tauri's own prerequisite list.
> - Do not attempt to cross-compile Windows from Linux or the reverse.
>
> Then audit the codebase for anything that will only work on Windows: path separator assumptions, WASAPI-specific behaviour that has no ALSA equivalent, MIDI port naming, and any hardcoded config directory. Report what you find before changing it.
>
> **Done when:** a green CI run on `ubuntu-22.04` gives evidence the Linux build works.

---

## Debugging template

Use this when timing goes wrong. Reach for a stronger model — root-cause investigation is
where it pays.

```powershell
claude --model fable      # or opus --effort xhigh
```

> Read CLAUDE.md and the relevant sections of docs/SPEC.md.
>
> Symptom: [describe exactly what you hear or see, and when it starts]
> Reproduction: [BPM, sample rate, song, section, how many minutes in]
> What I have already ruled out: [list]
>
> Investigate the root cause before proposing a fix. Use the offline renderer to reproduce this deterministically if you can — it is faster than listening and it gives you sample positions to compare against expected values. Show me the actual numbers, not a description of them.
>
> Do not change code until you can explain the mechanism.

---

## Session hygiene

- One phase per session. Fresh session between phases.
- If a session runs long and starts losing the thread, commit, exit, and start a new one
  pointing at the spec section you were on. Do not fight compaction.
- If Claude proposes something that contradicts CLAUDE.md, the invariants win — say so
  explicitly rather than accepting it, or the contradiction ends up in the codebase.
- Amend `docs/SPEC.md` when you learn something that changes the design. The spec is the
  source of truth across sessions; a decision that lives only in a chat transcript is lost.
