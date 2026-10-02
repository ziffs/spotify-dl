//! Persists the songs Spotify reported as unavailable.
//!
//! Every time a download run encounters an unavailable track, it is recorded
//! in `unavailable.json` in the dot path, keyed by the track URI, so
//! unavailable songs can be reviewed (and re-checked) afterwards.

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::Result;
use anyhow::anyhow;
use librespot::core::SpotifyUri;
use serde::Deserialize;
use serde::Serialize;

use crate::utils::get_dot_path;

#[derive(Debug, Default, Serialize, Deserialize)]
struct UnavailableTrack {
    /// Display name of the song, when its metadata was known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    error: String,
    /// Unix timestamp (seconds) of the last encounter.
    last_seen: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct UnavailableState {
    tracks: HashMap<String, UnavailableTrack>,
}

fn path() -> Result<PathBuf> {
    Ok(get_dot_path()?.join("unavailable.json"))
}

/// Records that the given song is unavailable, updating any previous entry.
pub(crate) fn record(id: &SpotifyUri, name: Option<&str>, error: &str) -> Result<()> {
    record_in(&path()?, id.to_string(), name, error)
}

fn record_in(path: &Path, id: String, name: Option<&str>, error: &str) -> Result<()> {
    let mut state: UnavailableState = fs::read_to_string(path)
        .ok()
        .and_then(|content| serde_json::from_str(&content).ok())
        .unwrap_or_default();

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|err| anyhow!("System clock error: {err}"))?
        .as_secs();
    state.tracks.insert(
        id,
        UnavailableTrack {
            name: name.map(|name| name.to_string()),
            error: error.to_string(),
            last_seen: now,
        },
    );

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    serde_json::to_writer_pretty(std::fs::File::create(path)?, &state)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn records_and_updates_unavailable_tracks() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("unavailable.json");
        let id = SpotifyUri::from_uri("spotify:track:0000000000000000000000").unwrap();

        record_in(
            &path,
            id.to_string(),
            Some("Song A"),
            "Track is unavailable",
        )
        .unwrap();
        // A later encounter updates the entry in place.
        record_in(
            &path,
            id.to_string(),
            Some("Song A"),
            "Track is unavailable (region)",
        )
        .unwrap();

        let state: UnavailableState =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(state.tracks.len(), 1);
        let track = state.tracks.get(&id.to_string()).unwrap();
        assert_eq!(track.name.as_deref(), Some("Song A"));
        assert_eq!(track.error, "Track is unavailable (region)");
        assert!(track.last_seen > 0);
    }

    #[test]
    fn missing_name_is_omitted_from_the_json() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("unavailable.json");
        let id = SpotifyUri::from_uri("spotify:track:0000000000000000000001").unwrap();

        record_in(&path, id.to_string(), None, "Track is unavailable").unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.contains("\"name\""), "{text}");
    }
}
