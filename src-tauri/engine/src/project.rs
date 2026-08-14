//! Project data model (`docs/SPEC.md` §7, §11).
//!
//! A project is a folder (`MySet.lsp/`), not a bare JSON file, so that all audio paths
//! can be relative and the folder survives being copied to a different machine. This
//! module defines the `project.json` schema; reading/writing the folder itself (audio
//! files, `cues/`, hashing) is a loader-phase concern and out of scope here.

use crate::error::{ProjectError, TimelineError};
use crate::path::RelPath;
use crate::timeline::TimeSignature;
use serde::{Deserialize, Serialize};

/// Current `project.json` schema version. Bump this and add a case to
/// [`migrate::upgrade`] whenever the format changes.
pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub schema_version: u32,
    pub name: String,
    /// The single project sample rate (44100 or 48000). All audio is resampled to
    /// this at load time; nothing is resampled at playback time (CLAUDE.md invariant 3).
    pub sample_rate: u32,
    #[serde(default)]
    pub bus_layout: BusLayout,
    #[serde(default)]
    pub click: ClickConfig,
    #[serde(default)]
    pub cue: CueConfig,
    #[serde(default)]
    pub songs: Vec<Song>,
}

impl Project {
    /// Validate cross-field invariants that serde's field-level defaults can't
    /// express: sample rate is one of the two supported rates, every song's tempo and
    /// section data is well-formed, and every track/click bus index is in range.
    pub fn validate(&self) -> Result<(), ProjectError> {
        if self.sample_rate != 44100 && self.sample_rate != 48000 {
            return Err(ProjectError::Validation(format!(
                "sample_rate must be 44100 or 48000, got {}",
                self.sample_rate
            )));
        }
        let bus_count = self.bus_layout.buses.len();
        if self.click.bus >= bus_count {
            return Err(ProjectError::Validation(format!(
                "click.bus {} is out of range for {} configured bus(es)",
                self.click.bus, bus_count
            )));
        }
        if self.cue.bus >= bus_count {
            return Err(ProjectError::Validation(format!(
                "cue.bus {} is out of range for {} configured bus(es)",
                self.cue.bus, bus_count
            )));
        }
        for (song_index, song) in self.songs.iter().enumerate() {
            song.validate(bus_count).map_err(|e| {
                ProjectError::Validation(format!("song {song_index} ('{}'): {e}", song.title))
            })?;
        }
        Ok(())
    }
}

/// N mono buses mapped to output channels (`docs/SPEC.md` §4). Defaults to two buses:
/// bus 0 "Backtrack" -> output channel 0, limiter on; bus 1 "Click/Cues" -> output
/// channel 1, limiter **off** (a limiter on the click bus would duck the click every
/// time a cue speaks).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BusLayout {
    pub buses: Vec<Bus>,
}

impl Default for BusLayout {
    fn default() -> Self {
        BusLayout {
            buses: vec![
                Bus {
                    name: "Backtrack".to_string(),
                    output_channel: 0,
                    limiter_enabled: true,
                },
                Bus {
                    name: "Click/Cues".to_string(),
                    output_channel: 1,
                    limiter_enabled: false,
                },
            ],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Bus {
    pub name: String,
    pub output_channel: u16,
    #[serde(default)]
    pub limiter_enabled: bool,
}

/// Project-wide click defaults: which bus the generated click routes to, and its
/// independent gain (separate from backtrack gain, per §5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClickConfig {
    #[serde(default = "default_click_bus")]
    pub bus: usize,
    #[serde(default)]
    pub gain_db: f64,
}

impl Default for ClickConfig {
    fn default() -> Self {
        ClickConfig {
            bus: default_click_bus(),
            gain_db: 0.0,
        }
    }
}

fn default_click_bus() -> usize {
    1
}

/// Project-wide spoken-cue defaults (`docs/SPEC.md` §8). Cues route to the same bus
/// as the click by default, but with independent gain -- a separate `Smoother` from
/// the click's, not a shared one (see `crate::core`). `voice_id` names an entry in
/// the app's (not project's) settings-level voice list -- a stable id, not a path, so
/// moving a project between machines doesn't change which voice it resolves to or
/// invalidate the cue content-hash cache (`crate::tts`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CueConfig {
    #[serde(default)]
    pub voice_id: String,
    #[serde(default = "default_cue_speed")]
    pub speed: f64,
    #[serde(default)]
    pub gain_db: f64,
    #[serde(default = "default_click_bus")]
    pub bus: usize,
}

impl Default for CueConfig {
    fn default() -> Self {
        CueConfig {
            voice_id: String::new(),
            speed: default_cue_speed(),
            gain_db: 0.0,
            bus: default_click_bus(),
        }
    }
}

fn default_cue_speed() -> f64 {
    1.0
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Song {
    pub id: String,
    pub title: String,
    pub bpm: f64,
    pub time_signature: TimeSignature,
    /// Where bar 1 beat 1 sits inside the backtrack file, in samples. Compensates for
    /// leading silence in a Reaper render. Applies only to source-frame lookups, never
    /// to performance-time scheduling (see `docs/SPEC.md` §2).
    #[serde(default)]
    pub offset_samples: i64,
    #[serde(default = "default_count_in_bars")]
    pub count_in_bars: u32,
    /// One intensity per pulse in the bar (length must equal `time_signature.numerator`
    /// once non-empty). Empty means "use the default pattern" (accent beat 1 only) --
    /// see [`crate::click::effective_accent_pattern`].
    #[serde(default)]
    pub accent_pattern: Vec<u8>,
    #[serde(default)]
    pub sections: Vec<Section>,
    #[serde(default)]
    pub tracks: Vec<Track>,
    #[serde(default)]
    pub disabled: bool,
}

fn default_count_in_bars() -> u32 {
    1
}

impl Song {
    fn validate(&self, bus_count: usize) -> Result<(), ProjectError> {
        self.time_signature
            .validate()
            .map_err(|e: TimelineError| ProjectError::Validation(e.to_string()))?;
        if !self.bpm.is_finite() || self.bpm <= 0.0 {
            return Err(ProjectError::Validation(format!(
                "bpm must be finite and positive, got {}",
                self.bpm
            )));
        }
        if self.count_in_bars > 4 {
            return Err(ProjectError::Validation(format!(
                "count_in_bars must be 0-4, got {}",
                self.count_in_bars
            )));
        }
        if !self.accent_pattern.is_empty()
            && self.accent_pattern.len() != self.time_signature.numerator as usize
        {
            return Err(ProjectError::Validation(format!(
                "accent_pattern has {} entries but time signature numerator is {}",
                self.accent_pattern.len(),
                self.time_signature.numerator
            )));
        }
        for (section_index, section) in self.sections.iter().enumerate() {
            if section.start_bar == 0 {
                return Err(ProjectError::Validation(format!(
                    "section {section_index} ('{}') has start_bar = 0; start_bar is 1-based",
                    section.name
                )));
            }
            if section.length_bars == 0 {
                return Err(ProjectError::Validation(format!(
                    "section {section_index} ('{}') has length_bars = 0",
                    section.name
                )));
            }
        }
        for (track_index, track) in self.tracks.iter().enumerate() {
            if track.bus >= bus_count {
                return Err(ProjectError::Validation(format!(
                    "track {track_index} ('{}') has bus {} out of range for {bus_count} configured bus(es)",
                    track.name, track.bus
                )));
            }
        }
        Ok(())
    }
}

/// A named region of the source backtrack (`docs/SPEC.md` §7). `start_bar` is 1-based
/// and refers to the **source audio**; the containing `Song::sections` list order is
/// unrelated to performance order -- performance order is supplied separately (see
/// [`crate::sections::PerformanceEntry`]) so reordering never touches this list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Section {
    pub name: String,
    /// 1-based bar number in the source backtrack.
    pub start_bar: u32,
    pub length_bars: u32,
    #[serde(default)]
    pub loopable: bool,
    /// `None` means "use the section name" (the default and common case).
    #[serde(default)]
    pub cue_text: Option<String>,
    #[serde(default = "default_cue_lead_beats")]
    pub cue_lead_beats: u32,
}

fn default_cue_lead_beats() -> u32 {
    4
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Track {
    pub id: String,
    pub name: String,
    pub file: AudioFileRef,
    #[serde(default)]
    pub gain_db: f64,
    #[serde(default)]
    pub muted: bool,
    #[serde(default)]
    pub bus: usize,
    #[serde(default)]
    pub downmix: DownmixMode,
    #[serde(default)]
    pub kind: TrackKind,
}

/// Reference to an audio file plus its verification data (`docs/SPEC.md` §11): a hash
/// and frame count captured at load time, so a re-exported backtrack with a changed
/// hash produces a clear warning instead of silently playing the wrong audio. Both are
/// `None` until the loader (a later phase) populates them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AudioFileRef {
    pub path: RelPath,
    #[serde(default)]
    pub sha256: Option<String>,
    #[serde(default)]
    pub frames: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum DownmixMode {
    #[default]
    Sum,
    Left,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TrackKind {
    #[default]
    Backtrack,
    Stem,
}

/// Parse `project.json`, probing `schema_version` first so an out-of-range or missing
/// version fails with a clear message before serde ever tries to parse the rest of the
/// document, and so older files can be migrated forward.
pub fn load_project_json(json: &str) -> Result<Project, ProjectError> {
    let value: serde_json::Value = serde_json::from_str(json)?;
    let found = value
        .get("schema_version")
        .and_then(|v| v.as_u64())
        .map(|v| v as u32);
    let found = match found {
        Some(0) | None => return Err(ProjectError::MissingSchemaVersion),
        Some(v) => v,
    };
    if found > SCHEMA_VERSION {
        return Err(ProjectError::SchemaTooNew {
            found,
            supported: SCHEMA_VERSION,
        });
    }
    let value = if found < SCHEMA_VERSION {
        migrate::upgrade(value, found)?
    } else {
        value
    };
    let project: Project = serde_json::from_value(value)?;
    Ok(project)
}

pub fn to_project_json(project: &Project) -> Result<String, ProjectError> {
    Ok(serde_json::to_string_pretty(project)?)
}

/// Schema migration. Empty today -- `SCHEMA_VERSION` is 1 and there is nothing older
/// to migrate from -- but the dispatch point exists from day one per §11, so the first
/// real migration is a new match arm here, not a new mechanism.
pub mod migrate {
    use super::ProjectError;
    use serde_json::Value;

    pub fn upgrade(value: Value, from_version: u32) -> Result<Value, ProjectError> {
        match from_version {
            v if v == super::SCHEMA_VERSION => Ok(value),
            other => Err(ProjectError::Validation(format!(
                "no migration path from schema_version {other} to {}",
                super::SCHEMA_VERSION
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timeline::TimeSignature;

    fn minimal_project_json() -> serde_json::Value {
        serde_json::json!({
            "schema_version": 1,
            "name": "Test Set",
            "sample_rate": 48000,
            "songs": [
                {
                    "id": "song1",
                    "title": "Disaster Kind",
                    "bpm": 178.0,
                    "time_signature": { "numerator": 4, "denominator": 4 },
                    "sections": [
                        {
                            "name": "Chorus",
                            "start_bar": 17,
                            "length_bars": 8
                        }
                    ]
                }
            ]
        })
    }

    #[test]
    fn parses_minimal_project_and_applies_section_defaults() {
        let json = minimal_project_json().to_string();
        let project = load_project_json(&json).unwrap();
        assert_eq!(project.schema_version, 1);
        assert_eq!(project.songs.len(), 1);
        let song = &project.songs[0];
        assert_eq!(song.offset_samples, 0);
        assert_eq!(song.count_in_bars, 1);
        assert!(song.accent_pattern.is_empty());
        let section = &song.sections[0];
        assert_eq!(section.cue_lead_beats, 4);
        assert!(!section.loopable);
        assert_eq!(section.cue_text, None);
        // §4 bus defaults.
        assert_eq!(project.bus_layout.buses.len(), 2);
        assert_eq!(project.bus_layout.buses[0].name, "Backtrack");
        assert!(project.bus_layout.buses[0].limiter_enabled);
        assert_eq!(project.bus_layout.buses[1].name, "Click/Cues");
        assert!(!project.bus_layout.buses[1].limiter_enabled);
        assert_eq!(project.click.bus, 1);
    }

    #[test]
    fn missing_schema_version_is_a_clear_error() {
        let mut json = minimal_project_json();
        json.as_object_mut().unwrap().remove("schema_version");
        let err = load_project_json(&json.to_string()).unwrap_err();
        assert!(matches!(err, ProjectError::MissingSchemaVersion));
    }

    #[test]
    fn zero_schema_version_is_treated_as_missing() {
        let mut json = minimal_project_json();
        json["schema_version"] = serde_json::json!(0);
        let err = load_project_json(&json.to_string()).unwrap_err();
        assert!(matches!(err, ProjectError::MissingSchemaVersion));
    }

    #[test]
    fn schema_version_too_new_is_a_clear_error() {
        let mut json = minimal_project_json();
        json["schema_version"] = serde_json::json!(999);
        let err = load_project_json(&json.to_string()).unwrap_err();
        match err {
            ProjectError::SchemaTooNew { found, supported } => {
                assert_eq!(found, 999);
                assert_eq!(supported, SCHEMA_VERSION);
            }
            other => panic!("expected SchemaTooNew, got {other:?}"),
        }
    }

    #[test]
    fn round_trips_through_json() {
        let json = minimal_project_json().to_string();
        let project = load_project_json(&json).unwrap();
        let serialized = to_project_json(&project).unwrap();
        let reparsed = load_project_json(&serialized).unwrap();
        assert_eq!(project, reparsed);
    }

    #[test]
    fn rejects_backslash_path_in_track_file() {
        let mut json = minimal_project_json();
        json["songs"][0]["tracks"] = serde_json::json!([
            {
                "id": "bt",
                "name": "Backtrack",
                "file": { "path": "audio\\disaster-kind.wav" }
            }
        ]);
        let err = load_project_json(&json.to_string());
        assert!(err.is_err());
    }

    #[test]
    fn rejects_absolute_path_in_track_file() {
        let mut json = minimal_project_json();
        json["songs"][0]["tracks"] = serde_json::json!([
            {
                "id": "bt",
                "name": "Backtrack",
                "file": { "path": "/audio/disaster-kind.wav" }
            }
        ]);
        assert!(load_project_json(&json.to_string()).is_err());
    }

    #[test]
    fn rejects_drive_letter_path_in_track_file() {
        let mut json = minimal_project_json();
        json["songs"][0]["tracks"] = serde_json::json!([
            {
                "id": "bt",
                "name": "Backtrack",
                "file": { "path": "C:/audio/disaster-kind.wav" }
            }
        ]);
        assert!(load_project_json(&json.to_string()).is_err());
    }

    #[test]
    fn rejects_parent_dir_path_in_track_file() {
        let mut json = minimal_project_json();
        json["songs"][0]["tracks"] = serde_json::json!([
            {
                "id": "bt",
                "name": "Backtrack",
                "file": { "path": "../audio/disaster-kind.wav" }
            }
        ]);
        assert!(load_project_json(&json.to_string()).is_err());
    }

    #[test]
    fn accepts_forward_slash_relative_path_in_track_file() {
        let mut json = minimal_project_json();
        json["songs"][0]["tracks"] = serde_json::json!([
            {
                "id": "bt",
                "name": "Backtrack",
                "file": { "path": "audio/disaster-kind.wav" }
            }
        ]);
        let project = load_project_json(&json.to_string()).unwrap();
        assert_eq!(
            project.songs[0].tracks[0].file.path.as_str(),
            "audio/disaster-kind.wav"
        );
    }

    #[test]
    fn validate_catches_bad_bus_index() {
        let mut json = minimal_project_json();
        json["songs"][0]["tracks"] = serde_json::json!([
            {
                "id": "bt",
                "name": "Backtrack",
                "file": { "path": "audio/disaster-kind.wav" },
                "bus": 5
            }
        ]);
        let project = load_project_json(&json.to_string()).unwrap();
        assert!(project.validate().is_err());
    }

    #[test]
    fn validate_catches_wrong_length_accent_pattern() {
        let mut json = minimal_project_json();
        json["songs"][0]["accent_pattern"] = serde_json::json!([2, 0, 0]); // 4/4 needs 4
        let project = load_project_json(&json.to_string()).unwrap();
        assert!(project.validate().is_err());
    }

    #[test]
    fn validate_catches_out_of_range_count_in_bars() {
        let mut json = minimal_project_json();
        json["songs"][0]["count_in_bars"] = serde_json::json!(5); // §6 range is 0-4
        let project = load_project_json(&json.to_string()).unwrap();
        assert!(project.validate().is_err());
    }

    #[test]
    fn validate_catches_bad_sample_rate() {
        let mut json = minimal_project_json();
        json["sample_rate"] = serde_json::json!(96000);
        let project = load_project_json(&json.to_string()).unwrap();
        assert!(project.validate().is_err());
    }

    #[test]
    fn validate_passes_on_well_formed_project() {
        let json = minimal_project_json().to_string();
        let project = load_project_json(&json).unwrap();
        assert!(project.validate().is_ok());
    }

    #[test]
    fn time_signature_accessible_from_project_module() {
        // sanity check the pub(crate) validation path used by Song::validate.
        assert!(TimeSignature::FOUR_FOUR.validate().is_ok());
    }
}
