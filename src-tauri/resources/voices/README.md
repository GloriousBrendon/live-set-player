# Bundled default voice

Like the Piper sidecar binaries (`src-tauri/binaries/README.md`), the default voice
model is **not committed to source control** — it's a ~60 MB binary, fully
reproducible from a pinned upstream URL, fetched by the release job before
`tauri build` runs (not by every `cargo build`, same reasoning as the sidecar).

## Pinned default voice

- Voice: `en_US-lessac-medium`, from
  [`huggingface.co/rhasspy/piper-voices`](https://huggingface.co/rhasspy/piper-voices)
  (§8: "Ship one voice model with the app").
- Files (verified via HTTP `HEAD`, not estimated):
  - `en_US-lessac-medium.onnx` — 63,201,294 bytes
  - `en_US-lessac-medium.onnx.json` — 4,885 bytes
- Fetch:
  ```sh
  curl -L -o en_US-lessac-medium.onnx \
    https://huggingface.co/rhasspy/piper-voices/resolve/main/en/en_US/lessac/medium/en_US-lessac-medium.onnx
  curl -L -o en_US-lessac-medium.onnx.json \
    https://huggingface.co/rhasspy/piper-voices/resolve/main/en/en_US/lessac/medium/en_US-lessac-medium.onnx.json
  ```
  Verify the `.onnx` download against the byte count above before bundling it — a
  truncated fetch would otherwise ship a voice that fails to load silently at
  first use rather than at build time.

This is a starting choice, not a locked-in one: any Piper voice from the same
`piper-voices` repo works identically (same VITS ONNX format, unaffected by which
Piper binary reads it). Revisit quality vs. size (a "low" quality voice is roughly a
third the size) once there's a UI to actually audition cues in.

User-added voices (§8: "allow the user to point at additional `.onnx` + `.json` voice
files in settings") live outside this directory entirely — this directory is only
ever the one voice shipped in the box.
