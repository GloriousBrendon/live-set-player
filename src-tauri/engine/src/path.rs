//! Relative, forward-slash path handling for project files.
//!
//! `docs/SPEC.md` §11: "A JSON file with absolute paths breaks the moment it lands on
//! the drummer's laptop." Every path stored in `project.json` is relative to the
//! project folder and uses forward slashes, regardless of the platform that saved it.
//! [`RelPath`] is the only type allowed to hold such a path, and [`RelPath::to_platform`]
//! / [`RelPath::from_platform`] are the only places that cross from this
//! platform-independent representation into a real filesystem `Path`.

use crate::error::RelPathError;
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};

/// A path relative to the project folder, stored and serialised as a forward-slash
/// string. Never absolute, never contains a backslash, never contains a `.` or `..`
/// component.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct RelPath(String);

impl RelPath {
    pub fn new(s: impl Into<String>) -> Result<Self, RelPathError> {
        let s = s.into();
        validate(&s)?;
        Ok(RelPath(s))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Join this relative path onto `root` to produce a real, platform-native path.
    /// The only place a `RelPath` becomes a filesystem `Path`.
    pub fn to_platform(&self, root: &Path) -> PathBuf {
        let mut out = root.to_path_buf();
        for segment in self.0.split('/') {
            out.push(segment);
        }
        out
    }

    /// Compute a `RelPath` from a real filesystem path plus the project root it should
    /// be relative to. The only place a filesystem `Path` becomes a `RelPath`.
    pub fn from_platform(root: &Path, path: &Path) -> Result<Self, RelPathError> {
        let relative = path
            .strip_prefix(root)
            .map_err(|_| RelPathError::Absolute(path.to_string_lossy().into_owned()))?;

        let mut segments = Vec::new();
        for component in relative.components() {
            match component {
                Component::Normal(part) => segments.push(part.to_string_lossy().into_owned()),
                Component::CurDir => {
                    return Err(RelPathError::DotComponent(
                        path.to_string_lossy().into_owned(),
                    ))
                }
                Component::ParentDir => {
                    return Err(RelPathError::DotComponent(
                        path.to_string_lossy().into_owned(),
                    ))
                }
                Component::RootDir | Component::Prefix(_) => {
                    return Err(RelPathError::Absolute(path.to_string_lossy().into_owned()))
                }
            }
        }
        if segments.is_empty() {
            return Err(RelPathError::Empty);
        }
        let joined = segments.join("/");
        validate(&joined)?;
        Ok(RelPath(joined))
    }
}

impl std::fmt::Display for RelPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for RelPath {
    type Error = RelPathError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        RelPath::new(s)
    }
}

impl<'de> Deserialize<'de> for RelPath {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        RelPath::new(s).map_err(serde::de::Error::custom)
    }
}

fn validate(s: &str) -> Result<(), RelPathError> {
    if s.is_empty() {
        return Err(RelPathError::Empty);
    }
    if s.contains('\\') {
        return Err(RelPathError::Backslash(s.to_string()));
    }
    if s.starts_with("//") {
        return Err(RelPathError::Unc(s.to_string()));
    }
    if s.starts_with('/') {
        return Err(RelPathError::Absolute(s.to_string()));
    }
    // Windows drive letter, e.g. "C:/audio/x.wav".
    let bytes = s.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        return Err(RelPathError::DriveLetter(s.to_string()));
    }
    for segment in s.split('/') {
        if segment == "." || segment == ".." {
            return Err(RelPathError::DotComponent(s.to_string()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_plain_relative_forward_slash_path() {
        let p = RelPath::new("audio/disaster-kind.wav").unwrap();
        assert_eq!(p.as_str(), "audio/disaster-kind.wav");
    }

    #[test]
    fn rejects_backslash() {
        assert_eq!(
            RelPath::new("audio\\disaster-kind.wav"),
            Err(RelPathError::Backslash("audio\\disaster-kind.wav".into()))
        );
    }

    #[test]
    fn rejects_leading_slash() {
        assert_eq!(
            RelPath::new("/audio/x.wav"),
            Err(RelPathError::Absolute("/audio/x.wav".into()))
        );
    }

    #[test]
    fn rejects_drive_letter() {
        assert_eq!(
            RelPath::new("C:/audio/x.wav"),
            Err(RelPathError::DriveLetter("C:/audio/x.wav".into()))
        );
    }

    #[test]
    fn rejects_parent_dir_component() {
        assert_eq!(
            RelPath::new("../audio/x.wav"),
            Err(RelPathError::DotComponent("../audio/x.wav".into()))
        );
        assert_eq!(
            RelPath::new("audio/../x.wav"),
            Err(RelPathError::DotComponent("audio/../x.wav".into()))
        );
    }

    #[test]
    fn rejects_unc_path() {
        assert_eq!(
            RelPath::new("//server/share/x.wav"),
            Err(RelPathError::Unc("//server/share/x.wav".into()))
        );
    }

    #[test]
    fn rejects_empty() {
        assert_eq!(RelPath::new(""), Err(RelPathError::Empty));
    }

    #[test]
    fn to_platform_joins_onto_root() {
        let p = RelPath::new("audio/disaster-kind.wav").unwrap();
        let root = Path::new("C:/Users/drummer/MySet.lsp");
        let joined = p.to_platform(root);
        assert_eq!(
            joined,
            Path::new("C:/Users/drummer/MySet.lsp")
                .join("audio")
                .join("disaster-kind.wav")
        );
    }

    #[test]
    fn from_platform_produces_forward_slash_relpath_on_any_os() {
        // Build a platform PathBuf the way Windows code would (via .join, which uses
        // the OS separator), then confirm the RelPath it produces is forward-slash
        // regardless of host OS.
        let root = Path::new("C:/Users/drummer/MySet.lsp");
        let mut full = root.to_path_buf();
        full.push("audio");
        full.push("disaster-kind.wav");

        let rel = RelPath::from_platform(root, &full).unwrap();
        assert_eq!(rel.as_str(), "audio/disaster-kind.wav");
        assert!(!rel.as_str().contains('\\'));
    }

    #[test]
    fn from_platform_rejects_path_outside_root() {
        let root = Path::new("C:/Users/drummer/MySet.lsp");
        let outside = Path::new("C:/Users/drummer/other/x.wav");
        assert!(RelPath::from_platform(root, outside).is_err());
    }

    #[test]
    fn serde_round_trip_is_a_bare_json_string() {
        let p = RelPath::new("audio/disaster-kind.wav").unwrap();
        let json = serde_json::to_string(&p).unwrap();
        assert_eq!(json, "\"audio/disaster-kind.wav\"");
        let back: RelPath = serde_json::from_str(&json).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn serde_rejects_backslash_on_deserialize() {
        let result: Result<RelPath, _> = serde_json::from_str("\"audio\\\\x.wav\"");
        assert!(result.is_err());
    }
}
