//! Cue/voice commands (`docs/SPEC.md` §8): bulk re-render and voice management.

use lsp_engine::config::CustomVoice;
use lsp_engine::tts;
use serde::Serialize;
use tauri::{AppHandle, State};

use crate::state::AppState;

#[derive(Debug, Clone, Serialize)]
pub struct CueSyncSummary {
    pub rendered: usize,
    pub cached: usize,
    pub failed: Vec<String>,
}

/// Bulk re-render every section's cue across the whole project (§8's "provide one
/// [render step] for bulk re-render", even though individual edits never need it).
#[tauri::command]
pub fn sync_all_cues(state: State<AppState>) -> Result<CueSyncSummary, String> {
    let guard = state.project.lock().unwrap();
    let ps = guard.as_ref().ok_or("no project loaded")?;
    let cues_dir = ps.dir.join("cues");
    let report = tts::sync_project_cues(&state.piper, &ps.project, &cues_dir, |id| {
        state.resolve_voice(id)
    });
    Ok(CueSyncSummary {
        rendered: report.rendered.len(),
        cached: report.cached.len(),
        failed: report
            .failed
            .iter()
            .map(|(r, e)| format!("song {}, section {}: {e}", r.song_id, r.section_index))
            .collect(),
    })
}

#[derive(Debug, Clone, Serialize)]
pub struct VoiceInfo {
    pub id: String,
}

#[tauri::command]
pub fn list_voices(state: State<AppState>) -> Vec<VoiceInfo> {
    state
        .voices
        .lock()
        .unwrap()
        .iter()
        .map(|v| VoiceInfo { id: v.id.clone() })
        .collect()
}

/// Add (or replace, if `id` already exists) a user-pointed voice (§8: "allow the user
/// to point at additional `.onnx` + `.json` voice files in settings"). Persisted to
/// `AppConfig` so it survives a restart, and the in-memory voice list is refreshed
/// immediately so it's usable without one.
#[tauri::command]
pub fn add_voice_file(
    app: AppHandle,
    state: State<AppState>,
    id: String,
    onnx_path: String,
    config_path: String,
) -> Result<(), String> {
    let custom_voices = {
        let mut cfg = state.config.lock().unwrap();
        cfg.custom_voices.retain(|v| v.id != id);
        cfg.custom_voices.push(CustomVoice {
            id,
            onnx_path,
            config_path,
        });
        cfg.save(&state.config_path).map_err(|e| e.to_string())?;
        cfg.custom_voices.clone()
    };
    *state.voices.lock().unwrap() = crate::sidecar::discover_voices(&app, &custom_voices);
    Ok(())
}
