//! App-level configuration (`docs/SPEC.md` §1, §11): the selected output device is
//! **app** config, persisted **by name** — never an index, never in the project file
//! (the drummer's laptop has different hardware).
//!
//! This crate stays Tauri-free, so the config file's *location* is the caller's
//! decision (the Tauri layer will use its app-config dir; the CLI example uses a
//! local path). Only the format and the no-fallback semantics live here.

use crate::error::DeviceError;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppConfig {
    /// The explicitly chosen output device, by name. `None` means "not configured
    /// yet" — which refuses to play; it never means "use the default device".
    #[serde(default)]
    pub output_device_name: Option<String>,
    /// Requested callback buffer size in frames. `None` uses the engine preference
    /// (1024). Values are clamped into the device's supported range at open time
    /// and never below 512 by preference — stability over latency.
    #[serde(default)]
    pub buffer_frames: Option<u32>,
    /// User-added cue voices beyond the one bundled with the app (`docs/SPEC.md` §8:
    /// "allow the user to point at additional `.onnx` + `.json` voice files in
    /// settings"). Absolute, platform-native paths -- unlike project audio paths,
    /// these live outside any project folder and never need to be portable.
    #[serde(default)]
    pub custom_voices: Vec<CustomVoice>,
}

/// One user-added voice, as chosen through a file picker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CustomVoice {
    /// Stable id used in the cue cache key (`crate::tts::cache_key`) and in a
    /// project's `CueConfig::voice_id` — must not collide with the bundled voice's id
    /// or another custom voice's.
    pub id: String,
    pub onnx_path: String,
    pub config_path: String,
}

impl AppConfig {
    /// Load from `path`; a missing file is a default config, any other error is
    /// surfaced (a corrupt config should be seen, not silently reset).
    pub fn load(path: &Path) -> Result<AppConfig, std::io::Error> {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(AppConfig::default()),
            Err(e) => Err(e),
        }
    }

    pub fn save(&self, path: &Path) -> Result<(), std::io::Error> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(path, text)
    }

    /// The configured device name, or the §1 refusal: no fallback, no default.
    pub fn require_device(&self) -> Result<&str, DeviceError> {
        self.output_device_name
            .as_deref()
            .ok_or(DeviceError::NoDeviceConfigured)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_loads_as_default() {
        let path =
            std::env::temp_dir().join(format!("lsp_config_missing_{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let cfg = AppConfig::load(&path).unwrap();
        assert_eq!(cfg, AppConfig::default());
        assert!(matches!(
            cfg.require_device(),
            Err(DeviceError::NoDeviceConfigured)
        ));
    }

    #[test]
    fn round_trips_device_name() {
        let path = std::env::temp_dir().join(format!("lsp_config_rt_{}.json", std::process::id()));
        let cfg = AppConfig {
            output_device_name: Some("Focusrite USB ASIO".into()),
            buffer_frames: Some(1024),
            custom_voices: vec![],
        };
        cfg.save(&path).unwrap();
        let loaded = AppConfig::load(&path).unwrap();
        assert_eq!(loaded, cfg);
        assert_eq!(loaded.require_device().unwrap(), "Focusrite USB ASIO");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn corrupt_file_is_an_error_not_a_silent_reset() {
        let path = std::env::temp_dir().join(format!("lsp_config_bad_{}.json", std::process::id()));
        std::fs::write(&path, "{not json").unwrap();
        assert!(AppConfig::load(&path).is_err());
        let _ = std::fs::remove_file(&path);
    }
}
