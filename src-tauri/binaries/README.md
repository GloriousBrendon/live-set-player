# Piper sidecar binaries

This directory is where the per-platform Piper TTS CLI binaries live at build time,
named per Tauri's `externalBin` convention: `piper-cli-<target-triple>[.exe]` (find
your triple with `rustc --print host-tuple`). **Binaries are never committed here** —
they're tens of MB, fully reproducible from the pinned source below, and belong to the
release lifecycle, not source control. The release CI job builds them fresh from the
pinned commit and places them here before `tauri build` runs.

`tauri.conf.json`'s `bundle.externalBin`/`bundle.resources` are **not** statically
committed either, and that's deliberate, not an oversight: Tauri's build script
validates those paths exist at *every* `cargo build` of the `lsp-scaffold` package,
including a bare local one — not only when actually bundling. Committing them
unconditionally would mean `cargo test --workspace` fails on any machine without the
sidecar already staged, i.e. every dev machine, which is exactly the everyday-build
friction this sidecar decision exists to avoid in the first place. The release
workflow patches them in with `jq` right before calling `tauri build` (see
`.github/workflows/ci.yml`'s "Wire the Piper sidecar into tauri.conf.json" step). If
you're working on the phase-5 Tauri command layer and need to exercise the sidecar
locally, build the binaries yourself per the steps below, place them here, and add the
same `externalBin`/`resources` block to your local `tauri.conf.json` — just don't
commit that edit.

See the TTS cues plan (`docs/SPEC.md` §8) for why this exists: `piper-rs` (the crate
SPEC originally named) turned out to require `libclang` + CMake on every machine that
runs `cargo build`, because it vendors and compiles espeak-ng via `bindgen`. That's an
unacceptable dev-loop burden, so we build a standalone Piper CLI once, in the release
job only, and ship it as a sidecar instead.

## Upstream, pinned

- Repo: [`OHF-Voice/piper1-gpl`](https://github.com/OHF-Voice/piper1-gpl) — GPL-3.0.
  This is the current upstream Piper (the older `rhasspy/piper` is unmaintained since
  2023-11-14 and was deliberately not used, even though it publishes prebuilt CLI
  zips with no build step — staying on current upstream mattered more than avoiding
  a CI build step).
- **Pinned tag:** `v1.6.0`
- **Pinned commit:** `f04d52c5528ac7cf2d73757f57990ff490f75005`
- **What we build:** the `libpiper` C++ CLI target (`libpiper/src/main/`), added in
  v1.5.0 ("libpiper C++ CLI executable ported from the legacy Piper repository"),
  with Windows (MSVC, MSYS2-GCC) and Linux builds confirmed working in that release's
  CI. It is **not** published as a release asset (`piper1-gpl` releases ship only
  Python wheels) — it has to be built from source, which is exactly why this is a CI
  build step rather than a downloaded artifact.
- Bump the pinned commit deliberately, not incidentally: re-verify the CLI still
  builds clean on both platforms and re-check the flag surface (below) before
  updating it.

## Build

**Verified against a real build on both platforms** (`verify-piper-sidecar`
workflow_dispatch job, 2026-08-14, commit `a4eb0fd`; re-run it and update this section
together whenever the pinned commit changes):

```sh
git clone https://github.com/OHF-Voice/piper1-gpl.git
cd piper1-gpl
git checkout f04d52c5528ac7cf2d73757f57990ff490f75005

# The CLI is libpiper/'s own CMake project, not the repo root's (that one builds an
# unrelated Python module and has no CLI target at all). CMAKE_INSTALL_PREFIX must be
# set at *configure* time -- one of libpiper's own install() rules bakes it in then,
# not at `cmake --install --prefix` time, and left at its default (/usr/local or
# Program Files) install fails with permission denied.
cmake -S libpiper -B build -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX="$PWD/install"

# `piper` is the shared *library* target; the CLI executable is the distinct
# `piper_exe` target (libpiper/src/main/CMakeLists.txt).
cmake --build build --config Release --target piper_exe
cmake --install build --config Release
```

This is a real build, not a trivial one: it downloads a prebuilt ONNX Runtime release
and builds espeak-ng from source via CMake's `ExternalProject_Add`. Budget real
minutes for it (each platform took a bit under 3 minutes total in the verify job,
after the one-time espeak-ng/onnxruntime download), not seconds.

**Runtime dependencies — this is not a single self-contained binary.**
`install/bin/piper_exe[.exe]` dynamically links two things that must ship alongside
it, in the *same directory* (confirmed the hard way: exit 127, "cannot open shared
object file", until each was staged):

- `install/piper.dll` / `install/libpiper.so` — the shared library `add_library(piper
  SHARED ...)` produces. Lands directly at the install prefix root, not under `bin/`
  (that particular `install()` rule has no `RUNTIME`/`LIBRARY` subdirectory keyword).
- `install/lib/onnxruntime.dll` (+ `onnxruntime_providers_shared.dll`) on Windows, or
  `install/lib/libonnxruntime.so*` (the versioned file plus its symlink chain) on
  Linux — `libpiper` links ONNX Runtime, which is *its* dynamic dependency.

On Linux, neither `piper_exe` nor `libpiper.so` carries an rpath by default, so the
loader won't find either sibling from an arbitrary install location — `patchelf
--set-rpath '$ORIGIN'` on both (`patchelf` is already a CI dependency for Tauri's own
AppImage bundling) makes each look next to itself first, so no environment variable
is needed at spawn time. Windows' default DLL search order already checks the
executable's own directory, so no equivalent step is needed there.

The CLI looks for `espeak-ng-data/` next to its own executable by default (confirmed);
`install/espeak-ng-data/` (also installed explicitly by `libpiper/CMakeLists.txt`) is
bundled as a Tauri resource (`src-tauri/resources/espeak-ng-data/`) rather than placed
next to the binary here — see `tauri.conf.json`'s `bundle.resources`.

**Confirmed against a real packaged bundle**, not just the CI build tree (phase-5,
Windows NSIS, `installMode: currentUser`, 2026-08-14): a real `tauri build` +
installed NSIS package puts `externalBin` (`piper-cli.exe`, `piper.dll`,
`onnxruntime.dll`, `onnxruntime_providers_shared.dll`) and `bundle.resources`
(`resources/espeak-ng-data/`, `resources/voices/`, `resources/licenses/`) in the
**same directory** — `%LOCALAPPDATA%\lsp-scaffold\` alongside `lsp-scaffold.exe`
itself, with `resources/` as a literal subdirectory there. `std::env::current_exe()`
`.parent()` (for the sidecar binary) and Tauri's `app.path().resource_dir()` (for
`espeak-ng-data`) both resolve to that same install root, so the app passes
`--espeak_data` explicitly (`src-tauri/src/sidecar.rs`) and it lands exactly where
`piper-cli.exe` also finds `piper.dll`/`onnxruntime.dll` via Windows' default
same-directory DLL search order. Verified by invoking the installed
`piper-cli.exe` directly with the app's exact resolved paths (exit 0, empty stderr,
valid output WAV) and by rendering a cue through the installed app itself. No
resource-layout fix was needed on Windows. **AppImage (Linux) is still unverified** —
this needs re-confirming on a Linux box; the `$ORIGIN` rpath and same-directory
staging *should* carry over the same way, but that's an expectation, not a check.

**CLI flags — confirmed via `--help` against the real build** (matches what was
already documented as a best-effort guess from the legacy `rhasspy/piper` CLI, so
`tts.rs` needed no changes):

```
usage: piper-cli [options]
options:
   -h        --help              show this message and exit
   -m  FILE  --model       FILE  path to onnx model file
   -c  FILE  --config      FILE  path to model config file (default: model path + .json)
   -f  FILE  --output_file FILE  path to output WAV file ('-' for stdout)
   -d  DIR   --output_dir  DIR   path to output directory (default: cwd)
   -s  NUM   --speaker     NUM   id of speaker (default: 0)
   --noise_scale           NUM   generator noise (default: 0.667)
   --length_scale          NUM   phoneme length (default: 1.0)
   --noise_w               NUM   phoneme width noise (default: 0.8)
   --espeak_data           DIR   path to espeak-ng data directory
   --json-input                  stdin input is lines of JSON instead of plain text
```
Text is read from stdin (confirmed: the verify job pipes `echo "chorus" | piper-cli
...`); on success the exe prints the output path to stdout and exits 0. Not currently
used by `tts.rs` but available if needed later: `--speaker` (multi-speaker voices),
`--noise_scale`/`--noise_w` (synthesis variation), `--json-input`.

**Two more things confirmed only by actually rendering through the packaged app**,
both handled in `src-tauri/engine/src/tts.rs`, neither visible from `--help` or a
one-off CLI run:

- The WAV this build writes carries placeholder `RIFF`/`data` chunk sizes (~2 GB,
  seemingly a "read until EOF" sentinel for streaming output) instead of the real
  byte counts. `hound` — used to decode every track, cues included, in
  `crate::loader` — takes the declared size at face value and fails with "Failed to
  read enough bytes" on a file that's actually kilobytes. `tts::render_cue` now
  rewrites both size fields to the file's real length immediately after a successful
  render (`fix_wav_header_sizes`, with a unit test reproducing the exact placeholder
  bytes observed).
- `piper-cli.exe` is a console-subsystem executable; spawning it from the
  windows-subsystem app flashed a visible console window on every single cue render
  until `piper_command` set the `CREATE_NO_WINDOW` process creation flag on Windows.

## Licensing

`piper1-gpl` (and the espeak-ng it vendors) is GPL-3.0. Shipping this sidecar binary
means the app distribution needs to carry that license, not just link to it:

- The verbatim `COPYING` file from the pinned commit is vendored at
  `src-tauri/resources/licenses/piper1-gpl-COPYING.txt` (copied byte-for-byte via
  `curl`, not retyped — SHA-256 `0ae0485a5bd37a63e63603596417e4eb0e653334fa6c7f932ca3a0e85d4af227`,
  675 lines) and bundled as a Tauri resource, so the shipped app itself carries the
  license text, not just this repo.
- Now confirmed exactly what gets bundled (see "Runtime dependencies" above), which
  narrows the **still-not-done** third-party-license work from a guess to a concrete
  list: espeak-ng's own license (MIT, though it carries GPL-licensed dictionary data
  in places) for `libespeak-ng`/`espeak-ng-data`, and Microsoft's ONNX Runtime license
  (MIT) for `onnxruntime.dll`/`libonnxruntime.so*` + `*providers_shared*`. Neither's
  license text is vendored yet, the way `piper1-gpl`'s `COPYING` is. At the pinned
  commit there is no `licenses/` directory upstream to copy from wholesale (one exists
  on `piper1-gpl`'s current `main`, added after `v1.6.0` — check whether a newer pin
  picks it up, or vendor espeak-ng's and onnxruntime's license text separately by the
  same byte-exact method used for `COPYING`). Resolve this before a public release;
  it's a compliance gap, not a code gap.
- GPL-3.0 source-availability: since we distribute the compiled binary, we need to be
  able to point at the exact source it was built from. The pinned tag/commit above
  *is* that pointer as long as it stays accurate — keep it in sync with whatever the
  release job actually builds.

## Voice models

Not this directory — bundled separately at `src-tauri/resources/voices/` (one default
voice shipped with the app; users can point at additional `.onnx`+`.json` pairs in
settings, per `docs/SPEC.md` §8). Piper voice models are unaffected by which Piper
binary reads them (same VITS ONNX format either way), sourced from
[`huggingface.co/rhasspy/piper-voices`](https://huggingface.co/rhasspy/piper-voices).
