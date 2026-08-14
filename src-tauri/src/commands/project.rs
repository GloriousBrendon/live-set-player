//! Project-folder and editor commands (`docs/SPEC.md` §7, §8, §11).
//!
//! Songs (and everything nested inside them -- sections, tracks) are edited
//! coarse-grained: the frontend holds the full `Song` locally, edits it, and calls
//! `update_song` with the result. That keeps the command surface small while still
//! covering every editor requirement (reorder sections, set flags, adjust offsets,
//! ...), and gives `update_song` one clear place to diff old vs. new section text and
//! trigger cue regeneration (§8: "renaming a section automatically triggers
//! regeneration... do not require a manual render-cues step").

use lsp_engine::loader::{self, VerifyWarning};
use lsp_engine::path::RelPath;
use lsp_engine::project::{
    AudioFileRef, BusLayout, ClickConfig, CueConfig, DownmixMode, Project, Song, Track, TrackKind,
};
use lsp_engine::timeline::TimeSignature;
use lsp_engine::tts;
use serde::Serialize;
use std::path::PathBuf;
use tauri::State;

use super::gen_id;
use crate::state::{AppState, ProjectState};

#[derive(Debug, Clone, Serialize)]
pub struct LoadProjectResult {
    pub project: Project,
    pub warnings: Vec<VerifyWarning>,
}

#[tauri::command]
pub fn load_project(state: State<AppState>, path: String) -> Result<LoadProjectResult, String> {
    let dir = PathBuf::from(path);
    let (project, warnings) = loader::load_project_folder(&dir).map_err(|e| e.to_string())?;
    project.validate().map_err(|e| e.to_string())?;
    *state.project.lock().unwrap() = Some(ProjectState {
        project: project.clone(),
        dir,
        current_song_index: None,
    });
    Ok(LoadProjectResult { project, warnings })
}

#[tauri::command]
pub fn save_project(state: State<AppState>) -> Result<(), String> {
    let mut guard = state.project.lock().unwrap();
    let ps = guard.as_mut().ok_or("no project loaded")?;
    loader::save_project_folder(&ps.dir, &mut ps.project).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn new_project(
    state: State<AppState>,
    path: String,
    name: String,
    sample_rate: u32,
) -> Result<Project, String> {
    let dir = PathBuf::from(path);
    let mut project = Project {
        schema_version: lsp_engine::project::SCHEMA_VERSION,
        name,
        sample_rate,
        bus_layout: BusLayout::default(),
        click: ClickConfig::default(),
        cue: CueConfig::default(),
        songs: vec![],
    };
    project.validate().map_err(|e| e.to_string())?;
    loader::save_project_folder(&dir, &mut project).map_err(|e| e.to_string())?;
    *state.project.lock().unwrap() = Some(ProjectState {
        project: project.clone(),
        dir,
        current_song_index: None,
    });
    Ok(project)
}

#[tauri::command]
pub fn get_project(state: State<AppState>) -> Option<Project> {
    state
        .project
        .lock()
        .unwrap()
        .as_ref()
        .map(|ps| ps.project.clone())
}

#[tauri::command]
pub fn update_project_settings(
    state: State<AppState>,
    name: String,
    click: ClickConfig,
    cue: CueConfig,
    bus_layout: BusLayout,
) -> Result<Project, String> {
    let mut guard = state.project.lock().unwrap();
    let ps = guard.as_mut().ok_or("no project loaded")?;
    let previous = ps.project.clone();
    ps.project.name = name;
    ps.project.click = click;
    ps.project.cue = cue;
    ps.project.bus_layout = bus_layout;
    if let Err(e) = ps.project.validate() {
        ps.project = previous;
        return Err(e.to_string());
    }
    Ok(ps.project.clone())
}

fn reorder_vec<T: Clone>(items: &mut Vec<T>, new_order: &[usize]) -> Result<(), String> {
    if new_order.len() != items.len() {
        return Err("reorder length does not match the current list length".to_string());
    }
    let mut seen = vec![false; items.len()];
    for &i in new_order {
        if i >= items.len() || seen[i] {
            return Err("reorder indices must be a permutation of the current list".to_string());
        }
        seen[i] = true;
    }
    let old = std::mem::take(items);
    *items = new_order.iter().map(|&i| old[i].clone()).collect();
    Ok(())
}

#[tauri::command]
pub fn add_song(state: State<AppState>) -> Result<Song, String> {
    let mut guard = state.project.lock().unwrap();
    let ps = guard.as_mut().ok_or("no project loaded")?;
    let song = Song {
        id: gen_id("song"),
        title: "New Song".to_string(),
        bpm: 120.0,
        time_signature: TimeSignature::FOUR_FOUR,
        offset_samples: 0,
        count_in_bars: 1,
        accent_pattern: vec![],
        sections: vec![],
        tracks: vec![],
        disabled: false,
    };
    ps.project.songs.push(song.clone());
    Ok(song)
}

#[tauri::command]
pub fn remove_song(state: State<AppState>, song_index: usize) -> Result<(), String> {
    let mut guard = state.project.lock().unwrap();
    let ps = guard.as_mut().ok_or("no project loaded")?;
    if song_index >= ps.project.songs.len() {
        return Err("song index out of range".to_string());
    }
    ps.project.songs.remove(song_index);
    if ps.current_song_index == Some(song_index) {
        ps.current_song_index = None;
    }
    Ok(())
}

#[tauri::command]
pub fn reorder_songs(state: State<AppState>, new_order: Vec<usize>) -> Result<(), String> {
    let mut guard = state.project.lock().unwrap();
    let ps = guard.as_mut().ok_or("no project loaded")?;
    reorder_vec(&mut ps.project.songs, &new_order)
}

#[derive(Debug, Clone, Serialize)]
pub struct UpdateSongResult {
    pub song: Song,
    /// Cue renders that failed after this edit (e.g. sidecar not staged yet in a dev
    /// build). Non-fatal to the edit itself -- §8 cue rendering is best-effort at
    /// edit time; a missing cue surfaces again at song-load time as a load error, per
    /// `loader::load_cue_bank`'s doc comment.
    pub cue_warnings: Vec<String>,
}

/// Replace `song_index`'s `Song` wholesale, validate the resulting project, and
/// regenerate the cue for any section whose effective text changed (renamed, or
/// `cue_text` edited) or that's new.
#[tauri::command]
pub fn update_song(
    state: State<AppState>,
    song_index: usize,
    song: Song,
) -> Result<UpdateSongResult, String> {
    let mut guard = state.project.lock().unwrap();
    let ps = guard.as_mut().ok_or("no project loaded")?;
    let existing = ps
        .project
        .songs
        .get(song_index)
        .ok_or("song index out of range")?;

    let old_texts: Vec<String> = existing
        .sections
        .iter()
        .map(|s| tts::effective_cue_text(s).to_string())
        .collect();

    let previous = existing.clone();
    ps.project.songs[song_index] = song;
    if let Err(e) = ps.project.validate() {
        ps.project.songs[song_index] = previous;
        return Err(e.to_string());
    }

    let mut cue_warnings = Vec::new();
    if let Some(voice) = state.resolve_voice(&ps.project.cue.voice_id) {
        let cues_dir = ps.dir.join("cues");
        let speed = ps.project.cue.speed;
        let new_song = &ps.project.songs[song_index];
        for (i, section) in new_song.sections.iter().enumerate() {
            let text = tts::effective_cue_text(section);
            if text.is_empty() {
                continue;
            }
            let changed = old_texts.get(i).is_none_or(|t| t != text);
            if changed {
                if let Err(e) = tts::ensure_cue(&state.piper, &cues_dir, &voice, text, speed) {
                    cue_warnings.push(format!("section '{}': {e}", section.name));
                }
            }
        }
    }

    Ok(UpdateSongResult {
        song: ps.project.songs[song_index].clone(),
        cue_warnings,
    })
}

/// Copy `source_path` into the project's `audio/` folder and append it as a new
/// track. Renaming on collision rather than overwriting -- two different tracks
/// legitimately named the same file on the source machine must not clobber each
/// other inside the project.
#[tauri::command]
pub fn add_track(
    state: State<AppState>,
    song_index: usize,
    source_path: String,
    name: String,
) -> Result<Track, String> {
    let mut guard = state.project.lock().unwrap();
    let ps = guard.as_mut().ok_or("no project loaded")?;
    let song = ps
        .project
        .songs
        .get_mut(song_index)
        .ok_or("song index out of range")?;

    let source = std::path::Path::new(&source_path);
    let audio_dir = ps.dir.join("audio");
    std::fs::create_dir_all(&audio_dir).map_err(|e| e.to_string())?;

    let stem = source
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "track".to_string());
    let ext = source
        .extension()
        .map(|e| e.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut dest = audio_dir.join(source.file_name().ok_or("invalid source path")?);
    let mut n = 1u32;
    while dest.exists() {
        let candidate = if ext.is_empty() {
            format!("{stem}-{n}")
        } else {
            format!("{stem}-{n}.{ext}")
        };
        dest = audio_dir.join(candidate);
        n += 1;
    }
    std::fs::copy(source, &dest).map_err(|e| e.to_string())?;

    let rel = RelPath::from_platform(&ps.dir, &dest).map_err(|e| e.to_string())?;
    let check = loader::verify_audio_file(&dest).map_err(|e| e.to_string())?;

    let track = Track {
        id: gen_id("track"),
        name,
        file: AudioFileRef {
            path: rel,
            sha256: Some(check.sha256),
            frames: Some(check.frames),
        },
        gain_db: 0.0,
        muted: false,
        bus: 0,
        downmix: DownmixMode::Sum,
        kind: TrackKind::Backtrack,
    };
    song.tracks.push(track.clone());
    Ok(track)
}

#[tauri::command]
pub fn remove_track(
    state: State<AppState>,
    song_index: usize,
    track_index: usize,
) -> Result<(), String> {
    let mut guard = state.project.lock().unwrap();
    let ps = guard.as_mut().ok_or("no project loaded")?;
    let song = ps
        .project
        .songs
        .get_mut(song_index)
        .ok_or("song index out of range")?;
    if track_index >= song.tracks.len() {
        return Err("track index out of range".to_string());
    }
    song.tracks.remove(track_index);
    Ok(())
}

#[tauri::command]
pub fn reorder_tracks(
    state: State<AppState>,
    song_index: usize,
    new_order: Vec<usize>,
) -> Result<(), String> {
    let mut guard = state.project.lock().unwrap();
    let ps = guard.as_mut().ok_or("no project loaded")?;
    let song = ps
        .project
        .songs
        .get_mut(song_index)
        .ok_or("song index out of range")?;
    reorder_vec(&mut song.tracks, &new_order)
}
