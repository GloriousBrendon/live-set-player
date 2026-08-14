//! Spoken-cue rendering (`docs/SPEC.md` §8): shells out to a prebuilt Piper CLI
//! sidecar and caches the result by content hash.
//!
//! This is worker-thread / edit-time code, driven by whatever calls `sync_project_cues`
//! -- never the audio thread, never at playback (CLAUDE.md invariant 1: the cues
//! themselves are just preloaded `Arc<[f32]>` by the time playback starts, same as
//! every other track; see [`crate::loader::load_cue_bank`]).
//!
//! No Tauri dependency, per this crate's rule (see `lib.rs`): the caller resolves the
//! actual sidecar binary path (a Tauri `externalBin`, once packaged --
//! `src-tauri/binaries/README.md` has the acquisition story) and the configured voice
//! paths, and passes them in as plain [`std::path::Path`]s. That keeps this module
//! testable with `std::process::Command` against a stub executable.
//!
//! **CLI flag names are confirmed against a real build**, not a guess: the
//! `verify-piper-sidecar` workflow_dispatch job (`.github/workflows/ci.yml`) builds
//! the pinned commit on both platforms and actually invokes it. `--model`, `--config`,
//! `--output_file`, `--length_scale`, `--espeak_data` (used by [`piper_command`]
//! below) all matched on the first real run, plus `-h`/`--help`, `-d`/`--output_dir`,
//! `-s`/`--speaker`, `--noise_scale`, `--noise_w`, and `--json-input` (not currently
//! used here). Text goes on stdin; on success the exe prints the output path to
//! stdout and exits 0 -- see `src-tauri/binaries/README.md` for the full `--help`
//! transcript and, more importantly, the runtime shared-library dependencies
//! (`libpiper.so`/`piper.dll` plus ONNX Runtime) the CI build now stages alongside
//! the executable, without which it fails to even start.
//!
//! Cache design: the cache key **is** the content hash of `(text, voice_id, speed)`.
//! That's what makes "renaming a section automatically triggers regeneration" require
//! no dirty-tracking at all -- a renamed section's effective text changes, so its hash
//! changes, so [`ensure_cue`] simply doesn't find a cached file under the new hash and
//! renders one. The old file under the old hash is left in `cues/`, orphaned but
//! harmless; garbage-collecting it is out of scope here (`docs/SPEC.md`-style known
//! limitation, not a TODO).

use crate::error::CueRenderError;
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Where a configured voice's model files live. `id` is the stable identifier used in
/// the cache key -- not the path -- so moving a project (or reinstalling voices at a
/// different location) doesn't change what's already cached (`docs/SPEC.md` §8: "Ship
/// one voice model with the app; allow the user to point at additional... voice files
/// in settings").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoicePaths {
    pub id: String,
    pub onnx_path: PathBuf,
    pub config_path: PathBuf,
}

/// Where the sidecar binary (and its `espeak-ng-data` directory, which the CLI looks
/// for next to the executable by default) are resolved to. Built by the Tauri layer
/// from `externalBin`/resource resolution; this crate never resolves paths itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PiperSidecar {
    pub binary_path: PathBuf,
    /// `None` lets the CLI fall back to its own default (`espeak-ng-data` next to the
    /// executable); set explicitly when that layout can't be guaranteed.
    pub espeak_data_dir: Option<PathBuf>,
}

/// SHA-256 hex digest of `(text, voice_id, speed)` -- the cue cache key. `speed` is
/// hashed via its bits, not a formatted string, so it can't collide across
/// float-formatting differences.
pub fn cache_key(text: &str, voice_id: &str, speed: f64) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    hasher.update([0u8]); // separator, so ("ab","c") and ("a","bc") can't collide
    hasher.update(voice_id.as_bytes());
    hasher.update([0u8]);
    hasher.update(speed.to_bits().to_le_bytes());
    let digest = hasher.finalize();
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// The cached WAV path for a given cache key, inside the project's `cues/` directory.
pub fn cached_cue_path(project_cues_dir: &Path, key: &str) -> PathBuf {
    project_cues_dir.join(format!("{key}.wav"))
}

/// Build the sidecar invocation. Text is passed on stdin (matching the legacy Piper
/// CLI); see module docs about verifying this against the actual pinned build.
fn piper_command(
    sidecar: &PiperSidecar,
    voice: &VoicePaths,
    speed: f64,
    out_path: &Path,
) -> Command {
    let mut cmd = Command::new(&sidecar.binary_path);
    cmd.arg("--model")
        .arg(&voice.onnx_path)
        .arg("--config")
        .arg(&voice.config_path)
        .arg("--output_file")
        .arg(out_path)
        .arg("--length_scale")
        .arg(format!("{speed}"));
    if let Some(espeak_data_dir) = &sidecar.espeak_data_dir {
        cmd.arg("--espeak_data").arg(espeak_data_dir);
    }
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd
}

/// Render `text` with `voice` at `speed` (1.0 = normal) to `out_path`, unconditionally
/// (no cache check -- see [`ensure_cue`] for the caching entry point). Every failure
/// mode is a distinct [`CueRenderError`] variant; there is no path through this
/// function that silently produces no file and returns `Ok`.
pub fn render_cue(
    sidecar: &PiperSidecar,
    voice: &VoicePaths,
    text: &str,
    speed: f64,
    out_path: &Path,
) -> Result<(), CueRenderError> {
    if !sidecar.binary_path.is_file() {
        return Err(CueRenderError::SidecarNotFound(
            sidecar.binary_path.display().to_string(),
        ));
    }
    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut child = piper_command(sidecar, voice, speed, out_path)
        .spawn()
        .map_err(|source| CueRenderError::SidecarSpawnFailed {
            path: sidecar.binary_path.display().to_string(),
            source,
        })?;

    // Write text to stdin and close it before waiting, or a CLI that reads to EOF
    // before synthesising would deadlock against our own `wait()`.
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(text.as_bytes());
    }

    let output = child
        .wait_with_output()
        .map_err(|source| CueRenderError::SidecarSpawnFailed {
            path: sidecar.binary_path.display().to_string(),
            source,
        })?;
    if !output.status.success() {
        return Err(CueRenderError::SidecarExitedWithError {
            status: output.status.to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    match std::fs::metadata(out_path) {
        Ok(meta) if meta.len() > 0 => Ok(()),
        _ => Err(CueRenderError::OutputUnreadable(
            out_path.display().to_string(),
        )),
    }
}

/// Cache-aware render: if a WAV already exists at the content-hash path, return it
/// without spawning the sidecar at all; otherwise render and cache it. This is the
/// function edit-time callers (rename, bulk re-render, save) should use.
pub fn ensure_cue(
    sidecar: &PiperSidecar,
    project_cues_dir: &Path,
    voice: &VoicePaths,
    text: &str,
    speed: f64,
) -> Result<PathBuf, CueRenderError> {
    let key = cache_key(text, &voice.id, speed);
    let path = cached_cue_path(project_cues_dir, &key);
    if path.is_file() {
        return Ok(path);
    }
    render_cue(sidecar, voice, text, speed, &path)?;
    Ok(path)
}

/// Identifies one song's section for reporting purposes (`docs/SPEC.md` project
/// format uses stable song ids, not indices, since songs can be reordered).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SectionRef {
    pub song_id: String,
    pub section_index: usize,
}

/// Result of a cue sync pass (`sync_project_cues`): which sections were freshly
/// rendered, which were already cached, and which failed -- with the specific error
/// for each. A caller (the future "save project" command, or an explicit bulk
/// re-render) must check `failed` and cannot express "it just worked" when it didn't;
/// there is no variant that means "skipped silently."
#[derive(Debug, Default)]
pub struct CueSyncReport {
    pub rendered: Vec<SectionRef>,
    pub cached: Vec<SectionRef>,
    pub failed: Vec<(SectionRef, CueRenderError)>,
}

impl CueSyncReport {
    pub fn is_all_ok(&self) -> bool {
        self.failed.is_empty()
    }
}

/// Effective cue text for a section: `cue_text` if set, else the section's own name
/// (`docs/SPEC.md` §8: "`cue_text: null` means use the section name").
pub fn effective_cue_text(section: &crate::project::Section) -> &str {
    section
        .cue_text
        .as_deref()
        .filter(|t| !t.is_empty())
        .unwrap_or(&section.name)
}

/// Render (or confirm cached) every section's cue across every song in `project`.
/// `resolve_voice` looks up a project's `cue.voice_id` against the caller's settings
/// -- this crate has no settings storage of its own. A section with empty effective
/// text (empty `cue_text` override *and* empty name -- a degenerate project state) is
/// skipped, not attempted.
pub fn sync_project_cues(
    sidecar: &PiperSidecar,
    project: &crate::project::Project,
    project_cues_dir: &Path,
    resolve_voice: impl Fn(&str) -> Option<VoicePaths>,
) -> CueSyncReport {
    let mut report = CueSyncReport::default();
    let Some(voice) = resolve_voice(&project.cue.voice_id) else {
        // No configured voice at all: every section that would need one fails
        // explicitly, rather than the sync silently doing nothing.
        for song in &project.songs {
            for (section_index, section) in song.sections.iter().enumerate() {
                if effective_cue_text(section).is_empty() {
                    continue;
                }
                report.failed.push((
                    SectionRef {
                        song_id: song.id.clone(),
                        section_index,
                    },
                    CueRenderError::VoiceNotFound(project.cue.voice_id.clone()),
                ));
            }
        }
        return report;
    };

    for song in &project.songs {
        for (section_index, section) in song.sections.iter().enumerate() {
            let text = effective_cue_text(section);
            if text.is_empty() {
                continue;
            }
            let section_ref = SectionRef {
                song_id: song.id.clone(),
                section_index,
            };
            let key = cache_key(text, &voice.id, project.cue.speed);
            let path = cached_cue_path(project_cues_dir, &key);
            if path.is_file() {
                report.cached.push(section_ref);
                continue;
            }
            match render_cue(sidecar, &voice, text, project.cue.speed, &path) {
                Ok(()) => report.rendered.push(section_ref),
                Err(e) => report.failed.push((section_ref, e)),
            }
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::{CueConfig, Section, Song, Track};
    use crate::timeline::TimeSignature;

    fn voice() -> VoicePaths {
        VoicePaths {
            id: "test-voice".into(),
            onnx_path: PathBuf::from("voice.onnx"),
            config_path: PathBuf::from("voice.onnx.json"),
        }
    }

    fn missing_sidecar() -> PiperSidecar {
        PiperSidecar {
            binary_path: PathBuf::from("this/path/does/not/exist/piper-cli"),
            espeak_data_dir: None,
        }
    }

    #[test]
    fn cache_key_changes_with_any_input() {
        let base = cache_key("Chorus", "voice-a", 1.0);
        assert_ne!(base, cache_key("Verse", "voice-a", 1.0));
        assert_ne!(base, cache_key("Chorus", "voice-b", 1.0));
        assert_ne!(base, cache_key("Chorus", "voice-a", 1.1));
        assert_eq!(base, cache_key("Chorus", "voice-a", 1.0));
    }

    /// Renaming a section changes its effective cue text, which changes the cache
    /// key -- this is the entire mechanism behind "renaming automatically triggers
    /// regeneration," exercised directly rather than through a rename-detection path
    /// that doesn't exist (and shouldn't need to).
    #[test]
    fn renamed_section_gets_a_different_cache_key() {
        let mut song = Song {
            id: "s1".into(),
            title: "Song".into(),
            bpm: 120.0,
            time_signature: TimeSignature::FOUR_FOUR,
            offset_samples: 0,
            count_in_bars: 0,
            accent_pattern: vec![],
            sections: vec![Section {
                name: "Chorus".into(),
                start_bar: 1,
                length_bars: 8,
                loopable: false,
                cue_text: None,
                cue_lead_beats: 4,
            }],
            tracks: Vec::<Track>::new(),
            disabled: false,
        };
        let before = cache_key(effective_cue_text(&song.sections[0]), "voice-a", 1.0);
        song.sections[0].name = "Final Chorus".into();
        let after = cache_key(effective_cue_text(&song.sections[0]), "voice-a", 1.0);
        assert_ne!(before, after);
    }

    #[test]
    fn effective_cue_text_falls_back_to_section_name() {
        let section = Section {
            name: "Bridge".into(),
            start_bar: 1,
            length_bars: 4,
            loopable: false,
            cue_text: None,
            cue_lead_beats: 4,
        };
        assert_eq!(effective_cue_text(&section), "Bridge");
        let mut overridden = section.clone();
        overridden.cue_text = Some("last time through the bridge".into());
        assert_eq!(
            effective_cue_text(&overridden),
            "last time through the bridge"
        );
    }

    #[test]
    fn missing_sidecar_binary_is_a_clear_error_not_a_panic() {
        let err = render_cue(
            &missing_sidecar(),
            &voice(),
            "Chorus",
            1.0,
            &std::env::temp_dir().join("lsp_tts_test_missing.wav"),
        )
        .unwrap_err();
        assert!(matches!(err, CueRenderError::SidecarNotFound(_)));
    }

    /// A cache hit must not spawn the sidecar at all -- verified by pointing at a
    /// sidecar path that would error loudly (via `SidecarNotFound`) if `render_cue`
    /// were reached, and confirming `ensure_cue` returns the existing file instead.
    #[test]
    fn cache_hit_never_invokes_the_sidecar() {
        let dir = std::env::temp_dir().join(format!("lsp_tts_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let key = cache_key("Chorus", "test-voice", 1.0);
        let cached_path = cached_cue_path(&dir, &key);
        std::fs::write(&cached_path, b"not a real wav, just needs to exist").unwrap();

        let result = ensure_cue(&missing_sidecar(), &dir, &voice(), "Chorus", 1.0).unwrap();
        assert_eq!(result, cached_path);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sync_project_cues_fails_clearly_with_no_configured_voice() {
        let project = crate::project::Project {
            schema_version: crate::project::SCHEMA_VERSION,
            name: "Test".into(),
            sample_rate: 48000,
            bus_layout: Default::default(),
            click: Default::default(),
            cue: CueConfig::default(), // voice_id left empty
            songs: vec![Song {
                id: "s1".into(),
                title: "Song".into(),
                bpm: 120.0,
                time_signature: TimeSignature::FOUR_FOUR,
                offset_samples: 0,
                count_in_bars: 0,
                accent_pattern: vec![],
                sections: vec![Section {
                    name: "Chorus".into(),
                    start_bar: 1,
                    length_bars: 8,
                    loopable: false,
                    cue_text: None,
                    cue_lead_beats: 4,
                }],
                tracks: Vec::<Track>::new(),
                disabled: false,
            }],
        };
        let dir = std::env::temp_dir().join(format!("lsp_tts_test_novoice_{}", std::process::id()));
        let report = sync_project_cues(&missing_sidecar(), &project, &dir, |_| None);
        assert!(!report.is_all_ok());
        assert_eq!(report.failed.len(), 1);
        assert!(matches!(
            report.failed[0].1,
            CueRenderError::VoiceNotFound(_)
        ));
    }
}
