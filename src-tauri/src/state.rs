//! Tauri-managed application state: the audio host channel, the shared engine
//! handle, the currently loaded project, app config, and resolved Piper/voice paths.

use lsp_engine::config::AppConfig;
use lsp_engine::midi::{Action, MidiBindingConfig, MidiRouter};
use lsp_engine::project::Project;
use lsp_engine::rt::EngineHandle;
use lsp_engine::tts::{PiperSidecar, VoicePaths};
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use crate::audio_host::{AudioHostMsg, OpenReport};
use crate::midi_host::MidiHostMsg;

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
    pub midi_tx: Sender<MidiHostMsg>,
    /// §9.2: bumped by every *human* transport action (play, stop, panic stop,
    /// arming a song) before the command reaches the engine. The setlist driver
    /// captures it when a song ends naturally and refuses to start the next song if
    /// it has changed since — so a stop during the inter-song gap always wins,
    /// with no dependence on poll timing.
    pub chain_epoch: Arc<AtomicU64>,
    /// Shared with the `midir` message callback running on the MIDI host thread
    /// (§9) -- it locks this on every incoming message to route/debounce or, in
    /// learn mode, to capture a new binding.
    pub midi: Arc<Mutex<MidiRuntimeState>>,
}

/// Runtime-only MIDI state (§9): the live binding router plus learn-mode progress.
/// Not part of `AppConfig` -- only `midi.router`'s bindings (via `AppConfig::
/// midi_bindings`) and the configured port name are persisted; `learn_pending` and
/// `last_learned` are UI-session state.
pub struct MidiRuntimeState {
    pub router: MidiRouter,
    /// Whether a MIDI input connection is currently open.
    pub open: bool,
    /// Set by `commands::midi::start_midi_learn`; the next message the callback
    /// parses binds to this action, overwriting any existing binding on that action
    /// or on that same message (§9: fast rebinding over collision warnings).
    pub learn_pending: Option<Action>,
    /// The most recent learn capture, surfaced to the settings UI until the next
    /// learn starts or the app restarts.
    pub last_learned: Option<MidiBindingConfig>,
}

impl MidiRuntimeState {
    pub fn new(bindings: Vec<MidiBindingConfig>) -> Self {
        Self {
            router: MidiRouter::new(bindings),
            open: false,
            learn_pending: None,
            last_learned: None,
        }
    }
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
