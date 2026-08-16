//! MIDI input commands (`docs/SPEC.md` §9): port enumeration/selection (persisted by
//! name, mirroring `commands/device.rs`), learn mode, and binding management.
//!
//! Learn mode and normal dispatch both run inside the `midir` callback given to
//! `lsp_engine::midi::open_midi_input` in [`select_midi_input_port`] below, on the
//! MIDI host thread (`midi_host.rs`). That callback:
//!
//! - locks `state.midi`,
//! - if `learn_pending` is set, parses the message (bypassing debounce -- learn
//!   mode wants the very first press, not a debounced one) and, on a successful
//!   parse, replaces any existing binding using that action *or* that message with
//!   the new one, persists it to `AppConfig`, and records it as `last_learned`;
//!   otherwise it feeds the router's normal `handle_bytes` and calls
//!   `commands::transport::dispatch` on a match.

use lsp_engine::config::AppConfig;
use lsp_engine::midi::{self, Action, MidiBindingConfig, ALL_ACTIONS};
use serde::Serialize;
use std::sync::{mpsc, Arc, Mutex};
use std::time::Instant;
use tauri::{AppHandle, Manager, State};

use crate::commands::transport;
use crate::midi_host::MidiHostMsg;
use crate::state::{AppState, MidiRuntimeState};

#[derive(Debug, Clone, Serialize)]
pub struct MidiStatus {
    pub configured_port: Option<String>,
    pub open: bool,
    pub bindings: Vec<MidiBindingConfig>,
    pub learn_pending: Option<Action>,
    pub last_learned: Option<MidiBindingConfig>,
}

#[tauri::command]
pub fn list_midi_input_ports() -> Result<Vec<String>, String> {
    midi::list_midi_input_ports().map_err(|e| e.to_string())
}

/// The callback handed to `midir`, run on the MIDI host thread for every message
/// while a port is open. Owns nothing itself -- everything it touches lives in
/// `AppState`, reached the same way a `#[tauri::command]` reaches it.
fn make_message_callback(app: AppHandle) -> impl FnMut(&[u8]) + Send + 'static {
    move |bytes: &[u8]| {
        let state = app.state::<AppState>();
        let mut midi_state = state.midi.lock().unwrap();

        if let Some(action) = midi_state.learn_pending {
            let Some(message) = midi::parse_midi_message(bytes) else {
                return;
            };
            midi_state.learn_pending = None;
            let binding = MidiBindingConfig { message, action };
            let new_bindings = replace_binding(midi_state.router.bindings(), binding);
            midi_state.router.set_bindings(new_bindings);
            midi_state.last_learned = Some(binding);
            let bindings = midi_state.router.bindings().to_vec();
            drop(midi_state);
            let mut cfg = state.config.lock().unwrap();
            cfg.midi_bindings = bindings;
            let _ = cfg.save(&state.config_path);
            return;
        }

        let Some(action) = midi_state.router.handle_bytes(bytes, Instant::now()) else {
            return;
        };
        drop(midi_state);
        let _ = transport::dispatch(&state, action);
    }
}

/// Bindings that shared `new`'s action or its message are dropped -- overwrite, not
/// collision-warn, per §9's "rebinding takes under a minute" bar.
fn replace_binding(
    existing: &[MidiBindingConfig],
    new: MidiBindingConfig,
) -> Vec<MidiBindingConfig> {
    let mut bindings: Vec<MidiBindingConfig> = existing
        .iter()
        .copied()
        .filter(|b| b.action != new.action && b.message != new.message)
        .collect();
    bindings.push(new);
    bindings
}

fn open_port(app: &AppHandle, state: &AppState, port_name: &str) -> Result<(), String> {
    let (tx, rx) = mpsc::channel();
    let callback = make_message_callback(app.clone());
    state
        .midi_tx
        .send(MidiHostMsg::Open {
            port_name: port_name.to_string(),
            on_message: Box::new(callback),
            reply: tx,
        })
        .map_err(|_| "MIDI host thread is gone".to_string())?;
    let result = rx
        .recv()
        .map_err(|_| "MIDI host thread did not respond".to_string())?;
    // The host thread drops any previous connection before attempting to open the
    // next one (`midi_host.rs`), so `open` must go false on failure too -- otherwise
    // a failed reopen would leave `MidiStatus` reporting a connection that no longer
    // exists.
    match result {
        Ok(()) => {
            state.midi.lock().unwrap().open = true;
            Ok(())
        }
        Err(e) => {
            state.midi.lock().unwrap().open = false;
            Err(e.to_string())
        }
    }
}

/// Select and persist (by name, per §9, mirroring §1's output-device persistence)
/// the MIDI input port, then open it immediately.
#[tauri::command]
pub fn select_midi_input_port(
    app: AppHandle,
    state: State<AppState>,
    name: String,
) -> Result<MidiStatus, String> {
    open_port(&app, &state, &name)?;
    {
        let mut cfg = state.config.lock().unwrap();
        cfg.midi_port_name = Some(name);
        cfg.save(&state.config_path).map_err(|e| e.to_string())?;
    }
    get_midi_status(state)
}

#[tauri::command]
pub fn get_midi_status(state: State<AppState>) -> Result<MidiStatus, String> {
    let configured_port = state.config.lock().unwrap().midi_port_name.clone();
    let midi_state = state.midi.lock().unwrap();
    Ok(MidiStatus {
        configured_port,
        open: midi_state.open,
        bindings: midi_state.router.bindings().to_vec(),
        learn_pending: midi_state.learn_pending,
        last_learned: midi_state.last_learned,
    })
}

/// Enter learn mode for `action`: the next note-on/CC/program-change message the
/// open port receives is bound to it (§9). Requires an open port -- there is nothing
/// to learn from otherwise.
#[tauri::command]
pub fn start_midi_learn(state: State<AppState>, action: Action) -> Result<(), String> {
    let mut midi_state = state.midi.lock().unwrap();
    if !midi_state.open {
        return Err("no MIDI port open; select one first".to_string());
    }
    midi_state.learn_pending = Some(action);
    Ok(())
}

#[tauri::command]
pub fn cancel_midi_learn(state: State<AppState>) -> Result<(), String> {
    state.midi.lock().unwrap().learn_pending = None;
    Ok(())
}

#[tauri::command]
pub fn remove_midi_binding(state: State<AppState>, action: Action) -> Result<MidiStatus, String> {
    {
        let mut midi_state = state.midi.lock().unwrap();
        let remaining: Vec<MidiBindingConfig> = midi_state
            .router
            .bindings()
            .iter()
            .copied()
            .filter(|b| b.action != action)
            .collect();
        midi_state.router.set_bindings(remaining.clone());
        let mut cfg = state.config.lock().unwrap();
        cfg.midi_bindings = remaining;
        cfg.save(&state.config_path).map_err(|e| e.to_string())?;
    }
    get_midi_status(state)
}

/// Every bindable action, for the settings UI to render one row per action even
/// before anything is bound.
#[tauri::command]
pub fn list_midi_actions() -> Vec<Action> {
    ALL_ACTIONS.to_vec()
}

/// Build the shared MIDI runtime state at startup from persisted config.
pub fn build_runtime_state(config: &AppConfig) -> Arc<Mutex<MidiRuntimeState>> {
    Arc::new(Mutex::new(MidiRuntimeState::new(
        config.midi_bindings.clone(),
    )))
}

/// Best-effort reopen of a previously configured MIDI port at startup (§9: unlike
/// the output device, a missing MIDI port is not a refusal -- keyboard shortcuts
/// keep working regardless).
pub fn reopen_configured_port(app: &AppHandle, state: &AppState) {
    let Some(name) = state.config.lock().unwrap().midi_port_name.clone() else {
        return;
    };
    let _ = open_port(app, state, &name);
}
