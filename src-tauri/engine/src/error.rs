//! Error types shared across the engine crate.

use thiserror::Error;

/// Errors from timeline / grid maths and section-order resolution.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum TimelineError {
    #[error("bpm must be finite and positive, got {0}")]
    InvalidBpm(String),
    #[error("time signature denominator must be a power of two, got {0}")]
    InvalidDenominator(u32),
    #[error("time signature numerator must be at least 1, got {0}")]
    InvalidNumerator(u32),
    #[error("sample rate must be nonzero")]
    InvalidSampleRate,
    #[error("performance entry {entry_index} references section index {section_index}, but the song only has {section_count} section(s)")]
    SectionIndexOutOfRange {
        entry_index: usize,
        section_index: usize,
        section_count: usize,
    },
    #[error("section {section_index} ('{name}') has length_bars = 0, which is not a valid section length")]
    ZeroLengthSection { section_index: usize, name: String },
    #[error("performance entry {entry_index} has repeats = 0, which would produce no audio; omit the entry instead")]
    ZeroRepeats { entry_index: usize },
    #[error("performance entry {entry_index} repeats section {section_index} ('{name}') {repeats} times, but that section isn't loopable; only a loopable section can stand in for live's \"repeat until advance\" (repeats > 1 on a non-loopable section can't correspond to anything a live performance would produce)")]
    RepeatsOnNonLoopableSection {
        entry_index: usize,
        section_index: usize,
        name: String,
        repeats: u32,
    },
    #[error("section {section_index} ('{name}') has start_bar = 0, but start_bar is 1-based and must be >= 1")]
    ZeroStartBar { section_index: usize, name: String },
}

/// Errors from relative-path construction (project file paths).
#[derive(Debug, Error, PartialEq, Eq)]
pub enum RelPathError {
    #[error("path '{0}' is absolute; project paths must be relative to the project folder")]
    Absolute(String),
    #[error("path '{0}' contains a Windows drive letter; project paths must be relative, forward-slash strings")]
    DriveLetter(String),
    #[error("path '{0}' is a UNC path; project paths must be relative, forward-slash strings")]
    Unc(String),
    #[error("path '{0}' contains a backslash; project paths must use forward slashes only")]
    Backslash(String),
    #[error("path '{0}' contains a '.' or '..' component, which is not allowed in project paths")]
    DotComponent(String),
    #[error("path is empty")]
    Empty,
}

/// Errors from project (de)serialisation and validation.
#[derive(Debug, Error)]
pub enum ProjectError {
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("project.json is missing a schema_version field")]
    MissingSchemaVersion,
    #[error("project.json has schema_version {found}, but this build only supports up to {supported}; update the app")]
    SchemaTooNew { found: u32, supported: u32 },
    #[error("path error: {0}")]
    Path(#[from] RelPathError),
    #[error("validation failed: {0}")]
    Validation(String),
}

/// Errors from the load-time audio pipeline (decode, downmix, resample).
#[derive(Debug, Error)]
pub enum LoadError {
    #[error("I/O error reading '{path}': {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("WAV decode error in '{path}': {source}")]
    Wav {
        path: String,
        #[source]
        source: hound::Error,
    },
    #[error("unsupported WAV format in '{path}': {detail}")]
    UnsupportedFormat { path: String, detail: String },
    #[error("resampling failed: {0}")]
    Resample(String),
}

/// Errors from the audio device layer (`docs/SPEC.md` §1).
#[derive(Debug, Error)]
pub enum DeviceError {
    #[error("no output device configured; pick one in settings (never falls back to a default)")]
    NoDeviceConfigured,
    #[error("configured output device '{name}' not found; available: {available:?}")]
    DeviceNotFound {
        name: String,
        available: Vec<String>,
    },
    #[error(
        "device does not support the project sample rate {requested} Hz; supported rates: {supported:?}"
    )]
    UnsupportedRate {
        requested: u32,
        supported: Vec<(u32, u32)>,
    },
    #[error("device offers no output config with a supported sample format")]
    NoUsableConfig,
    #[error("audio backend error: {0}")]
    Backend(String),
}

/// Errors from spoken-cue rendering (`docs/SPEC.md` §8): shelling out to the Piper
/// sidecar and caching the result. Every failure has a distinct variant on purpose --
/// a caller matching on this can only ever report a specific, actionable reason, never
/// silently treat "couldn't render" as "no cue needed."
#[derive(Debug, Error)]
pub enum CueRenderError {
    #[error("Piper sidecar binary not found at '{0}'; the app bundle is missing it or is corrupt")]
    SidecarNotFound(String),
    #[error("failed to spawn the Piper sidecar at '{path}': {source}")]
    SidecarSpawnFailed {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("Piper sidecar exited with status {status}: {stderr}")]
    SidecarExitedWithError { status: String, stderr: String },
    #[error("Piper sidecar reported success but no readable WAV was produced at '{0}'")]
    OutputUnreadable(String),
    #[error("cue voice '{0}' is not configured; add it in settings")]
    VoiceNotFound(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Errors from the offline renderer.
#[derive(Debug, Error)]
pub enum RenderError {
    #[error(transparent)]
    Timeline(#[from] TimelineError),
    #[error("track '{0}' referenced by song but not present in the audio bank")]
    MissingTrack(String),
    #[error("bus index {0} is out of range for the project's bus layout ({1} bus(es))")]
    BusIndexOutOfRange(usize, usize),
    #[error("I/O error writing WAV: {0}")]
    Io(#[from] std::io::Error),
    #[error("WAV encode error: {0}")]
    Hound(#[from] hound::Error),
}
