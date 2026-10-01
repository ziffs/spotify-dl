use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::Result;
use anyhow::anyhow;
use serde::Deserialize;
use serde::Serialize;

use crate::account::Folder;
use crate::account::collect_all_uris;
use crate::utils::get_dot_path;

/// Persistent state of the `--from-account` picker, stored as
/// `account_state.json` in the dot path.
///
/// It caches the account's folder tree and playlist names (so the picker can
/// open offline and refresh on demand), remembers the selection between runs
/// (pre-selected on the next start) and tracks which playlists were already
/// downloaded.
#[derive(Debug, Default, Serialize, Deserialize)]
pub(crate) struct AccountState {
    /// The account's folder tree (root-level loose playlists included).
    #[serde(default)]
    pub(crate) root: Folder,
    /// Playlist display names by URI.
    #[serde(default)]
    pub(crate) names: HashMap<String, String>,
    /// Playlist URIs currently selected in the picker.
    #[serde(default)]
    pub(crate) selected: Vec<String>,
    /// Playlist URIs a download run has completed, by unix timestamp (seconds).
    #[serde(default)]
    pub(crate) downloaded: HashMap<String, u64>,
}

impl AccountState {
    fn path() -> Result<PathBuf> {
        Ok(get_dot_path()?.join("account_state.json"))
    }

    pub(crate) fn load() -> Option<AccountState> {
        let path = Self::path().ok()?;
        Self::load_from(&path)
    }

    pub(crate) fn save(&self) -> Result<()> {
        self.save_to(&Self::path()?)
    }

    /// Records the given playlists as downloaded (now) and persists the state.
    pub(crate) fn mark_downloaded(uris: &[String]) -> Result<()> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|err| anyhow!("System clock error: {err}"))?
            .as_secs();

        let mut state = Self::load().unwrap_or_default();
        for uri in uris {
            state.downloaded.insert(uri.clone(), now);
        }
        state.save()
    }

    fn load_from(path: &Path) -> Option<AccountState> {
        let content = std::fs::read_to_string(path).ok()?;
        match serde_json::from_str(&content) {
            Ok(state) => Some(state),
            Err(err) => {
                tracing::warn!(
                    "Ignoring corrupted account state file {}: {err}",
                    path.display()
                );
                None
            }
        }
    }

    fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::File::create(path)?;
        serde_json::to_writer_pretty(file, self)?;
        Ok(())
    }

    /// Whether the cached tree contains at least one playlist.
    pub(crate) fn has_playlists(&self) -> bool {
        let mut uris = Vec::new();
        collect_all_uris(&self.root, &mut uris);
        !uris.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn state_with_selected(selected: &[&str]) -> AccountState {
        AccountState {
            selected: selected.iter().map(|uri| uri.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn roundtrips_through_disk() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("account_state.json");

        let mut state = state_with_selected(&["spotify:playlist:a", "spotify:playlist:b"]);
        state
            .names
            .insert("spotify:playlist:a".to_string(), "A".to_string());
        state
            .downloaded
            .insert("spotify:playlist:b".to_string(), 1234);

        state.save_to(&path).unwrap();
        let loaded = AccountState::load_from(&path).unwrap();

        assert_eq!(loaded.selected, state.selected);
        assert_eq!(loaded.names, state.names);
        assert_eq!(loaded.downloaded, state.downloaded);
    }

    #[test]
    fn missing_or_corrupted_file_loads_as_none() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("account_state.json");
        assert!(AccountState::load_from(&path).is_none());

        std::fs::write(&path, "not json at all").unwrap();
        assert!(AccountState::load_from(&path).is_none());
    }

    #[test]
    fn empty_tree_is_not_usable_offline() {
        let state = AccountState::default();
        assert!(!state.has_playlists());

        let mut state = AccountState::default();
        state.root.playlists.push("spotify:playlist:a".to_string());
        assert!(state.has_playlists());
    }
}
