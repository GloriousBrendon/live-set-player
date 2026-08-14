//! Output device commands (`docs/SPEC.md` §1): enumerate, select (persisted by name,
//! never by index), and report status -- including mid-session device loss.

use lsp_engine::device;
use serde::Serialize;
use std::sync::mpsc;
use tauri::State;

use crate::audio_host::AudioHostMsg;
use crate::state::AppState;

#[derive(Debug, Clone, Serialize)]
pub struct DeviceStatus {
    pub configured_device: Option<String>,
    pub open: bool,
    pub engine_rate: Option<u32>,
    pub channels: Option<u16>,
    /// §1 informational notice (Windows project/device rate mismatch), if any.
    pub notice: Option<String>,
    /// §1: 1 = device no longer available, 2 = other stream error. `None` = fine.
    pub error: Option<u32>,
}

#[tauri::command]
pub fn list_output_devices() -> Result<Vec<String>, String> {
    device::list_output_devices().map_err(|e| e.to_string())
}

fn open_device(state: &AppState, device_name: &str, project_rate: u32) -> Result<(), String> {
    let (tx, rx) = mpsc::channel();
    state
        .audio_tx
        .send(AudioHostMsg::Open {
            device_name: device_name.to_string(),
            project_rate,
            reply: tx,
        })
        .map_err(|_| "audio host thread is gone".to_string())?;
    let report = rx
        .recv()
        .map_err(|_| "audio host thread did not respond".to_string())?
        .map_err(|e| e.to_string())?;
    *state.last_open.lock().unwrap() = Some(report);
    Ok(())
}

/// Select and persist (by name, per §1 -- never by index, never a silent default)
/// the output device, then open it immediately so the UI reflects the real state.
#[tauri::command]
pub fn select_output_device(state: State<AppState>, name: String) -> Result<DeviceStatus, String> {
    let project_rate = state
        .project
        .lock()
        .unwrap()
        .as_ref()
        .map(|p| p.project.sample_rate)
        .unwrap_or(48000);

    open_device(&state, &name, project_rate)?;

    {
        let mut cfg = state.config.lock().unwrap();
        cfg.output_device_name = Some(name);
        cfg.save(&state.config_path).map_err(|e| e.to_string())?;
    }

    get_device_status(state)
}

#[tauri::command]
pub fn get_device_status(state: State<AppState>) -> Result<DeviceStatus, String> {
    let configured_device = state.config.lock().unwrap().output_device_name.clone();
    let is_open = state.engine.lock().unwrap().is_some();
    let last = state.last_open.lock().unwrap().clone();

    let error = if is_open {
        let (tx, rx) = mpsc::channel();
        if state
            .audio_tx
            .send(AudioHostMsg::CheckError { reply: tx })
            .is_ok()
        {
            rx.recv().ok().flatten()
        } else {
            None
        }
    } else {
        None
    };

    Ok(DeviceStatus {
        configured_device,
        open: is_open,
        engine_rate: last.as_ref().map(|r| r.engine_rate),
        channels: last.as_ref().map(|r| r.channels),
        notice: last.and_then(|r| r.notice),
        error,
    })
}
