mod audio_host;
mod commands;
mod sidecar;
mod state;

use audio_host::AudioHostMsg;
use state::AppState;
use std::sync::{Arc, Mutex};
use tauri::Manager;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let handle = app.handle().clone();

            let config_dir = handle
                .path()
                .app_config_dir()
                .expect("no app config directory available");
            std::fs::create_dir_all(&config_dir).ok();
            let config_path = config_dir.join("config.json");
            let config = lsp_engine::config::AppConfig::load(&config_path).unwrap_or_default();

            let piper = sidecar::resolve_piper_sidecar(&handle);
            let voices = sidecar::discover_voices(&handle, &config.custom_voices);

            let engine = Arc::new(Mutex::new(None));
            let audio_tx = audio_host::spawn(engine.clone());
            let last_open = Mutex::new(None);

            // §1: never fall back to a default device, but if one was already
            // configured on a previous run, open it now so the device status banner
            // has something real to show before any project is loaded. 48000 is a
            // placeholder project rate for this probe only -- WASAPI shared mode
            // ignores it entirely, and on Linux the real project rate re-opens the
            // device the moment a project loads (see commands::project::load_project
            // and commands::device::select_output_device).
            if let Some(name) = config.output_device_name.clone() {
                let (tx, rx) = std::sync::mpsc::channel();
                let _ = audio_tx.send(AudioHostMsg::Open {
                    device_name: name,
                    project_rate: 48000,
                    reply: tx,
                });
                if let Ok(Ok(report)) = rx.recv() {
                    *last_open.lock().unwrap() = Some(report);
                }
            }

            app.manage(AppState {
                audio_tx,
                engine,
                project: Mutex::new(None),
                config: Mutex::new(config),
                config_path,
                piper,
                voices: Mutex::new(voices),
                last_open,
            });

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::transport::get_status,
            commands::transport::play,
            commands::transport::stop,
            commands::transport::panic_stop,
            commands::transport::arm_section,
            commands::transport::seek_to_section,
            commands::transport::advance_section,
            commands::transport::set_track_gain,
            commands::transport::set_track_muted,
            commands::transport::set_track_bus,
            commands::transport::set_click_gain,
            commands::transport::set_limiter_enabled,
            commands::transport::set_count_in_override,
            commands::transport::arm_song,
            commands::transport::arm_next_song,
            commands::project::load_project,
            commands::project::save_project,
            commands::project::new_project,
            commands::project::get_project,
            commands::project::update_project_settings,
            commands::project::add_song,
            commands::project::remove_song,
            commands::project::reorder_songs,
            commands::project::update_song,
            commands::project::add_track,
            commands::project::remove_track,
            commands::project::reorder_tracks,
            commands::device::list_output_devices,
            commands::device::select_output_device,
            commands::device::get_device_status,
            commands::cues::sync_all_cues,
            commands::cues::list_voices,
            commands::cues::add_voice_file,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
