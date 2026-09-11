//! Transport commands (`docs/SPEC.md` §7, §9): thin wrappers over
//! `EngineHandle::send`/`latest_status`, plus the two "load a song for playback"
//! actions (`arm_song`, `arm_next_song`) that bridge the project model to the engine.
//!
//! The five actions MIDI bindings and keyboard shortcuts both trigger (§9: "arm next
//! song", "play", "advance section", "stop", "panic stop") go through [`dispatch`],
//! which the `dispatch_action` command below and the MIDI host's message callback
//! (`midi_host.rs`, via `commands::midi`) both call directly -- the same function,
//! not just equivalent logic, so a footswitch and its keyboard-shortcut counterpart
//! are indistinguishable to the engine.

use lsp_engine::loader;
use lsp_engine::midi::Action;
use lsp_engine::project::Song;
use lsp_engine::rt::{Command, Status};
use std::sync::atomic::Ordering;
use tauri::State;

use crate::state::AppState;

/// §9.2: mark a human transport action, cancelling any auto-continue the setlist
/// driver has pending. Called *before* the command is sent, so the driver can never
/// observe a stale epoch and start the next song after a stop.
fn cancel_pending_chain(state: &AppState) {
    state.chain_epoch.fetch_add(1, Ordering::SeqCst);
}

/// Non-mutating status read for the setlist driver (`setlist_driver.rs`). `None`
/// when no device is open or the engine has not published a snapshot yet.
pub fn peek_status(state: &AppState) -> Option<Status> {
    let mut guard = state.engine.lock().unwrap();
    guard.as_mut()?.latest_status()
}

/// Arm the next enabled song on behalf of the setlist driver. Deliberately does
/// **not** bump the chain epoch: this is the chain continuing, not a human
/// overriding it.
pub fn arm_next_song_for_chain(state: &AppState) -> Result<Option<Song>, String> {
    arm_next_song_impl(state)
}

/// Start playback on behalf of the setlist driver. Same reasoning as
/// [`arm_next_song_for_chain`] — no epoch bump.
pub fn play_for_chain(state: &AppState) -> Result<(), String> {
    send_command(state, Command::Play)
}

fn send_command(state: &AppState, cmd: Command) -> Result<(), String> {
    let mut guard = state.engine.lock().unwrap();
    let handle = guard
        .as_mut()
        .ok_or("no output device open; select one in settings")?;
    handle
        .send(cmd)
        .map_err(|_| "command queue is full; try again".to_string())
}

fn current_engine_rate(state: &AppState) -> Result<u32, String> {
    {
        let mut guard = state.engine.lock().unwrap();
        if let Some(handle) = guard.as_mut() {
            if let Some(status) = handle.latest_status() {
                return Ok(status.engine_rate);
            }
        }
    }
    state
        .last_open
        .lock()
        .unwrap()
        .as_ref()
        .map(|r| r.engine_rate)
        .ok_or_else(|| "engine rate not yet known; no device has been opened".to_string())
}

/// Polled by the frontend at ~30 Hz (`docs/SPEC.md` §9). `None` means no output
/// device is open yet -- not an error, just "nothing to show."
#[tauri::command]
pub fn get_status(state: State<AppState>) -> Option<Status> {
    state
        .engine
        .lock()
        .unwrap()
        .as_mut()
        .and_then(|h| h.latest_status())
}

fn dispatch_play(state: &AppState) -> Result<(), String> {
    cancel_pending_chain(state);
    send_command(state, Command::Play)
}

fn dispatch_stop(state: &AppState) -> Result<(), String> {
    cancel_pending_chain(state);
    send_command(state, Command::Stop)
}

fn dispatch_panic_stop(state: &AppState) -> Result<(), String> {
    cancel_pending_chain(state);
    send_command(state, Command::PanicStop)
}

fn dispatch_advance_section(state: &AppState) -> Result<(), String> {
    send_command(state, Command::AdvanceSection)
}

fn dispatch_arm_next_song(state: &AppState) -> Result<(), String> {
    cancel_pending_chain(state);
    arm_next_song_impl(state).map(|_| ())
}

/// The single entry point every §9 control input (keyboard, MIDI) routes through for
/// these five actions -- see the module doc comment.
pub fn dispatch(state: &AppState, action: Action) -> Result<(), String> {
    match action {
        Action::Play => dispatch_play(state),
        Action::Stop => dispatch_stop(state),
        Action::AdvanceSection => dispatch_advance_section(state),
        Action::ArmNextSong => dispatch_arm_next_song(state),
        Action::PanicStop => dispatch_panic_stop(state),
    }
}

#[tauri::command]
pub fn dispatch_action(state: State<AppState>, action: Action) -> Result<(), String> {
    dispatch(&state, action)
}

#[tauri::command]
pub fn play(state: State<AppState>) -> Result<(), String> {
    dispatch_play(&state)
}

#[tauri::command]
pub fn stop(state: State<AppState>) -> Result<(), String> {
    dispatch_stop(&state)
}

#[tauri::command]
pub fn panic_stop(state: State<AppState>) -> Result<(), String> {
    dispatch_panic_stop(&state)
}

#[tauri::command]
pub fn arm_section(state: State<AppState>, section: usize) -> Result<(), String> {
    send_command(&state, Command::ArmSection(section))
}

#[tauri::command]
pub fn seek_to_section(state: State<AppState>, section: usize) -> Result<(), String> {
    send_command(&state, Command::SeekToSection(section))
}

#[tauri::command]
pub fn advance_section(state: State<AppState>) -> Result<(), String> {
    dispatch_advance_section(&state)
}

#[tauri::command]
pub fn set_track_gain(state: State<AppState>, track: usize, db: f32) -> Result<(), String> {
    send_command(&state, Command::SetTrackGainDb { track, db })
}

#[tauri::command]
pub fn set_track_muted(state: State<AppState>, track: usize, muted: bool) -> Result<(), String> {
    send_command(&state, Command::SetTrackMuted { track, muted })
}

#[tauri::command]
pub fn set_track_bus(state: State<AppState>, track: usize, bus: usize) -> Result<(), String> {
    send_command(&state, Command::SetTrackBus { track, bus })
}

#[tauri::command]
pub fn set_click_gain(state: State<AppState>, db: f32) -> Result<(), String> {
    send_command(&state, Command::SetClickGainDb(db))
}

#[tauri::command]
pub fn set_limiter_enabled(
    state: State<AppState>,
    bus: usize,
    enabled: bool,
) -> Result<(), String> {
    send_command(&state, Command::SetLimiterEnabled { bus, enabled })
}

#[tauri::command]
pub fn set_count_in_override(state: State<AppState>, bars: Option<u32>) -> Result<(), String> {
    send_command(&state, Command::SetCountInOverride(bars))
}

fn arm_song_impl(state: &AppState, song_id: &str) -> Result<Song, String> {
    let engine_rate = current_engine_rate(state)?;

    let (dir, project, song) = {
        let guard = state.project.lock().unwrap();
        let ps = guard.as_ref().ok_or("no project loaded")?;
        let song = ps
            .project
            .songs
            .iter()
            .find(|s| s.id == song_id)
            .ok_or_else(|| format!("song '{song_id}' not found"))?
            .clone();
        (ps.dir.clone(), ps.project.clone(), song)
    };
    let loaded =
        loader::load_and_prepare(&dir, &project, &song, engine_rate).map_err(|e| e.to_string())?;

    send_command(state, Command::LoadSong(loaded))?;
    send_command(state, Command::ArmSection(0))?;

    let mut guard = state.project.lock().unwrap();
    if let Some(ps) = guard.as_mut() {
        ps.current_song_index = ps.project.songs.iter().position(|s| s.id == song_id);
    }
    Ok(song)
}

/// Load `song_id`'s audio and arm its first section, ready for `play`.
#[tauri::command]
pub fn arm_song(state: State<AppState>, song_id: String) -> Result<Song, String> {
    cancel_pending_chain(&state);
    arm_song_impl(&state, &song_id)
}

/// Arm the next enabled song after the current one in setlist order (§9: "arm next
/// song"). `Ok(None)` when there is no next song, not an error -- reaching the end of
/// the set is a normal thing to happen at a gig.
fn arm_next_song_impl(state: &AppState) -> Result<Option<Song>, String> {
    let next_id = {
        let guard = state.project.lock().unwrap();
        let ps = guard.as_ref().ok_or("no project loaded")?;
        let start = ps.current_song_index.map(|i| i + 1).unwrap_or(0);
        ps.project
            .songs
            .iter()
            .enumerate()
            .skip(start)
            .find(|(_, s)| !s.disabled)
            .map(|(_, s)| s.id.clone())
    };
    match next_id {
        Some(id) => arm_song_impl(state, &id).map(Some),
        None => Ok(None),
    }
}

#[tauri::command]
pub fn arm_next_song(state: State<AppState>) -> Result<Option<Song>, String> {
    cancel_pending_chain(&state);
    arm_next_song_impl(&state)
}
