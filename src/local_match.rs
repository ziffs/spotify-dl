//! Matching `spotify:local:` playlist entries to files on disk.
//!
//! Local playlist entries are not Spotify catalog tracks, so they cannot be
//! downloaded. Instead the user points the app at a folder that may contain
//! the files, and each entry is matched against the folder's files: first
//! with a `<artist> - <title>*` glob, then — if nothing matches — with a
//! fuzzier glob where every non-alphanumeric character became a `*`, so
//! files named with underscores, dashes or different parentheses are found
//! too. Confirmed matches are persisted and reused in later runs.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use librespot::core::SpotifyUri;

use crate::account::decode_group_name;

/// A `spotify:local:` playlist entry, with its fields decoded.
#[derive(Debug, Clone)]
pub struct LocalTrackInfo {
    pub artist: String,
    pub title: String,
}

impl LocalTrackInfo {
    /// How the song is presented to the user.
    pub fn display(&self) -> String {
        if self.artist.is_empty() {
            self.title.clone()
        } else {
            format!("{} - {}", self.artist, self.title)
        }
    }
}

/// Whether the URI refers to a local file instead of a Spotify catalog track.
pub(crate) fn is_local(uri: &SpotifyUri) -> bool {
    matches!(uri, SpotifyUri::Local { .. })
}

/// Parses and decodes a `spotify:local:` URI.
pub(crate) fn parse_local(uri: &SpotifyUri) -> Option<LocalTrackInfo> {
    let SpotifyUri::Local {
        artist,
        album_title: _,
        track_title,
        duration: _,
    } = uri
    else {
        return None;
    };
    Some(LocalTrackInfo {
        artist: decode_group_name(artist),
        title: decode_group_name(track_title),
    })
}

/// Case-insensitive glob match supporting `*` (any sequence) only.
fn glob_match(pattern: &str, text: &str) -> bool {
    let pattern = pattern.to_lowercase();
    let text = text.to_lowercase();

    let mut segments = pattern.split('*');
    let first = segments.next().unwrap_or("");
    let Some(mut rest) = text.strip_prefix(first) else {
        return false;
    };

    let parts: Vec<&str> = segments.collect();
    let Some((last, middles)) = parts.split_last() else {
        // No wildcard: the pattern is a prefix of the text.
        return true;
    };
    for middle in middles {
        if middle.is_empty() {
            continue;
        }
        match rest.find(*middle) {
            Some(position) => rest = &rest[position + middle.len()..],
            None => return false,
        }
    }
    rest.ends_with(last)
}

/// Turns every non-alphanumeric character into a `*` wildcard, so files named
/// with underscores, dashes or different parentheses still match.
fn fuzzy_glob(text: &str) -> String {
    let wild: String = text
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '*' })
        .collect();
    format!("*{wild}*")
}

/// Files in `folder` that look like the given local entry: first with the
/// `<artist> - <title>*` glob, then — when nothing matches — with the fuzzier
/// glob. Non-recursive, files only, sorted by name.
pub(crate) fn find_candidates(folder: &Path, info: &LocalTrackInfo) -> Result<Vec<PathBuf>> {
    let mut candidates = Vec::new();
    // `<artist> - <title>*` — the display name is already "artist - title".
    let primary = format!("{}*", info.display());

    let collect = |pattern: &str, candidates: &mut Vec<PathBuf>| -> Result<()> {
        for entry in fs::read_dir(folder)? {
            let entry = entry?;
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if glob_match(pattern, &name) {
                candidates.push(path);
            }
        }
        Ok(())
    };

    collect(&primary, &mut candidates)?;
    if candidates.is_empty() {
        collect(&fuzzy_glob(primary.trim_end_matches('*')), &mut candidates)?;
    }

    candidates.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
    Ok(candidates)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local_uri(raw: &str) -> SpotifyUri {
        SpotifyUri::from_uri(raw).unwrap()
    }

    #[test]
    fn parses_and_decodes_local_uris() {
        let info = parse_local(&local_uri(
            "spotify:local:OSIVE+x+THNK+PNK::CHOP+SUEY+%28EDIT%29:134",
        ))
        .unwrap();
        assert_eq!(info.artist, "OSIVE x THNK PNK");
        assert_eq!(info.title, "CHOP SUEY (EDIT)");
        assert_eq!(info.display(), "OSIVE x THNK PNK - CHOP SUEY (EDIT)");
        assert!(is_local(&local_uri(
            "spotify:local:OSIVE+x+THNK+PNK::CHOP+SUEY+%28EDIT%29:134"
        )));
        assert!(!is_local(&local_uri(
            "spotify:track:0000000000000000000000"
        )));
    }

    #[test]
    fn glob_matches_prefixes_and_wildcards() {
        // The primary pattern from the user's example.
        assert!(glob_match(
            "osive x thnk pnk - chop suey (edit*",
            "OSIVE x THNK PNK - CHOP SUEY (EDIT).mp3"
        ));
        // The fuzzy pattern finds underscore variants.
        assert!(glob_match(
            "*osive*thnk*pnk*chop*suey*edit*",
            "OSIVE_x_THNK_PNK_-_CHOP_SUEY_(EDIT).mp3"
        ));
        // Fragments must appear in order.
        assert!(!glob_match(
            "*chop*suey*osive*",
            "OSIVE x THNK PNK - CHOP SUEY.mp3"
        ));
        // A pattern without a wildcard is a prefix match.
        assert!(glob_match("abc", "abcdef"));
        assert!(!glob_match("abc", "xabc"));
    }

    #[test]
    fn finds_candidates_with_primary_then_fuzzy_glob() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path();
        fs::write(folder.join("OSIVE x THNK PNK - CHOP SUEY (EDIT).mp3"), b"x").unwrap();
        fs::write(folder.join("OSIVE_x_THNK_PNK_-_CHOP_SUEY_(EDIT).mp3"), b"x").unwrap();
        fs::write(folder.join("unrelated.mp3"), b"x").unwrap();
        fs::create_dir(folder.join("subdir")).unwrap();

        let info = parse_local(&local_uri(
            "spotify:local:OSIVE+x+THNK+PNK::CHOP+SUEY+%28EDIT%29:134",
        ))
        .unwrap();

        // The primary glob matches the space-separated file name.
        let candidates = find_candidates(folder, &info).unwrap();
        assert_eq!(candidates.len(), 1);
        assert!(
            candidates[0]
                .file_name()
                .unwrap()
                .to_string_lossy()
                .contains("(EDIT).mp3")
        );

        // When nothing matches the primary glob, the fuzzy glob is used.
        let dir2 = tempfile::tempdir().unwrap();
        fs::write(
            dir2.path().join("osive_x_thnk_pnk_chop_suey_edit.mp3"),
            b"x",
        )
        .unwrap();
        fs::write(dir2.path().join("unrelated.mp3"), b"x").unwrap();
        let candidates = find_candidates(dir2.path(), &info).unwrap();
        assert_eq!(candidates.len(), 1);
        assert!(
            candidates[0]
                .file_name()
                .unwrap()
                .to_string_lossy()
                .contains("osive")
        );
    }
}
