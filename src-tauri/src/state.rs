//! Tauri-managed application state: the audio host channel, the shared engine
//! handle, the currently loaded project, app config, and resolved Piper/voice paths.

use lsp_engine::config::AppConfig;
use lsp_engine::project::Project;
use lsp_engine::rt::EngineHandle;
use lsp_engine::tts::{PiperSidecar, VoicePaths};
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use crate::audio_host::{AudioHostMsg, OpenReport};

/// The project currently open in the editor, plus the runtime-only "what's armed for
/// playback" state that isn't part of `project.json` (`docs/SPEC.md` §7: performance
/// order/position is separate from the section list itself).
pub struct ProjectState {
    pub project: Project,
    pub dir: PathBuf,
    pub current_song_index: Option<usize>,
}

pub struct AppState {
    pub audio_tx: Sender<AudioHostMsg>,
    pub engine: Arc<Mutex<Option<EngineHandle>>>,
    pub project: Mutex<Option<ProjectState>>,
    pub config: Mutex<AppConfig>,
    pub config_path: PathBuf,
    pub piper: PiperSidecar,
    pub voices: Mutex<Vec<VoicePaths>>,
    /// Result of the last successful device open, cached so `get_device_status` can
    /// report engine rate/channels/notice without round-tripping the audio host.
    pub last_open: Mutex<Option<OpenReport>>,
}

impl AppState {
    /// Look up a configured voice by id, falling back to the first available voice
    /// when `voice_id` is empty or unknown (so a freshly created project with no
    /// voice chosen yet still renders cues with *something* rather than failing
    /// every render with `VoiceNotFound`).
    pub fn resolve_voice(&self, voice_id: &str) -> Option<VoicePaths> {
        let voices = self.voices.lock().unwrap();
        voices
            .iter()
            .find(|v| v.id == voice_id)
            .or_else(|| voices.first())
            .cloned()
    }
}
