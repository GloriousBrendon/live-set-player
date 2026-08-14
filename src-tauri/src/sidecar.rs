//! Resolves the packaged Piper sidecar binary and voice files (`docs/SPEC.md` §8).
//!
//! `lsp_engine::tts` deliberately takes plain, already-resolved paths -- it has no
//! Tauri dependency and no opinion about `externalBin`/resource layout. This is the
//! one place that opinion lives.

use lsp_engine::config::CustomVoice;
use lsp_engine::tts::{PiperSidecar, VoicePaths};
use std::path::PathBuf;
use tauri::{AppHandle, Manager};

/// Resolve the sidecar binary and its `espeak-ng-data` directory.
///
/// The binary is a Tauri `externalBin`, which lands beside the main executable in
/// both the NSIS and AppImage bundle layouts -- `current_exe().parent()` is that
/// directory in a packaged app and in `tauri dev` alike (in dev it simply won't exist
/// until the sidecar is staged locally per `src-tauri/binaries/README.md`, which
/// surfaces as `CueRenderError::SidecarNotFound`, not a panic).
///
/// `espeak_data_dir` is passed explicitly rather than relying on Piper's own
/// next-to-executable default, because the packaged resource directory
/// (`app.path().resource_dir()`) is not guaranteed to be the same directory the
/// `externalBin` lands in -- see `src-tauri/binaries/README.md` and the phase-5
/// packaged-build verification step for what was actually confirmed.
pub fn resolve_piper_sidecar(app: &AppHandle) -> PiperSidecar {
    let binary_name = if cfg!(windows) {
        "piper-cli.exe"
    } else {
        "piper-cli"
    };
    let binary_path = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join(binary_name)))
        .unwrap_or_else(|| PathBuf::from(binary_name));

    let espeak_data_dir = app
        .path()
        .resource_dir()
        .ok()
        .map(|dir| dir.join("resources").join("espeak-ng-data"));

    PiperSidecar {
        binary_path,
        espeak_data_dir,
    }
}

/// Scan the bundled `resources/voices/` directory for `<name>.onnx` +
/// `<name>.onnx.json` pairs, then append the user's custom voices from `AppConfig`
/// (§8: "allow the user to point at additional voice files in settings"). Bundled
/// voices come first so a custom voice with a colliding id simply shadows it in the
/// returned list, which callers resolve by scanning in order.
pub fn discover_voices(app: &AppHandle, custom: &[CustomVoice]) -> Vec<VoicePaths> {
    let mut voices = Vec::new();

    if let Ok(resource_dir) = app.path().resource_dir() {
        let voices_dir = resource_dir.join("resources").join("voices");
        if let Ok(entries) = std::fs::read_dir(&voices_dir) {
            for entry in entries.flatten() {
                let onnx_path = entry.path();
                if onnx_path.extension().and_then(|e| e.to_str()) != Some("onnx") {
                    continue;
                }
                let config_path = onnx_path.with_extension("onnx.json");
                if !config_path.is_file() {
                    continue;
                }
                let Some(id) = onnx_path.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                voices.push(VoicePaths {
                    id: id.to_string(),
                    onnx_path,
                    config_path,
                });
            }
        }
    }

    for v in custom {
        voices.push(VoicePaths {
            id: v.id.clone(),
            onnx_path: PathBuf::from(&v.onnx_path),
            config_path: PathBuf::from(&v.config_path),
        });
    }

    voices
}
