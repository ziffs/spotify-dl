use std::collections::HashMap;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use anyhow::Result;
use anyhow::anyhow;
use bytes::Bytes;
use futures::StreamExt;
use librespot::core::SpotifyUri;
use librespot::core::session::Session;
use librespot::core::spotify_id::SpotifyId;
use librespot::protocol::playlist4_external::SelectedListContent;
use protobuf::Message;
use serde::Deserialize;
use serde::Serialize;

use crate::account_state::AccountState;
use crate::capture::CaptureStore;
use crate::capture::MockStore;
use crate::capture::ROOTLIST_PAGE_SIZE;
use crate::folder_picker;
use crate::folder_picker::Outcome;

/// Hard cap on rootlist entries, so a misbehaving server cannot make us loop forever.
const MAX_ROOTLIST_ITEMS: usize = 10_000;

/// Where the account data (rootlist, playlist metadata) comes from.
///
/// [`AccountSource::Live`] talks to Spotify through the session and can
/// optionally record every raw response with a [`CaptureStore`] (see the
/// `SPOTIFY_DL_CAPTURE_DIR` environment variable); [`AccountSource::Mock`]
/// serves previously captured and anonymized fixtures, running the whole
/// flow offline.
pub enum AccountSource {
    Live {
        session: Session,
        capture: Option<CaptureStore>,
    },
    Mock {
        store: MockStore,
    },
}

impl AccountSource {
    pub fn live(session: Session, capture_dir: Option<PathBuf>) -> Self {
        Self::Live {
            session,
            capture: capture_dir.map(CaptureStore::new),
        }
    }

    pub fn mock(dir: impl Into<PathBuf>) -> Self {
        Self::Mock {
            store: MockStore::new(dir),
        }
    }

    async fn rootlist_page(&self, from: usize, length: usize) -> Result<Bytes> {
        match self {
            AccountSource::Live { session, capture } => {
                let response = session.spclient().get_rootlist(from, Some(length)).await?;
                if let Some(capture) = capture
                    && let Err(err) = capture.save_rootlist_page(from, length, &response)
                {
                    tracing::warn!("Could not capture the rootlist response: {err:#}");
                }
                Ok(response)
            }
            AccountSource::Mock { store } => store.rootlist_page(from, length),
        }
    }

    async fn playlist_bytes(&self, id: &SpotifyId) -> Result<Bytes> {
        match self {
            AccountSource::Live { session, capture } => {
                let response = session.spclient().get_playlist(id).await?;
                if let Some(capture) = capture
                    && let Err(err) = capture.save_playlist(id, &response)
                {
                    tracing::warn!("Could not capture the playlist response: {err:#}");
                }
                Ok(response)
            }
            AccountSource::Mock { store } => store.playlist_bytes(id),
        }
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Folder {
    pub name: String,
    pub playlists: Vec<String>,
    pub children: Vec<Folder>,
}

enum RootlistItem {
    FolderStart(String),
    FolderEnd,
    Playlist(String),
    Other,
}

/// Queries the account's rootlist (the sidebar contents), shows an interactive
/// tree view of its folders and playlists, and returns the URIs the user
/// selected, so the caller can proceed as if they had been passed as download
/// arguments.
///
/// The selection, the folder tree, the playlist names and the downloaded
/// status are persisted in the dot path: the selection is pre-selected on the
/// next start, and the cache is used when the account cannot be reached.
pub async fn select_folder_playlists(source: &AccountSource) -> Result<Vec<String>> {
    let cached = AccountState::load().filter(|state| state.has_playlists());

    let (root, mut names) = match fetch_account_data(source, false).await {
        Ok(data) => data,
        Err(err) => match &cached {
            Some(state) => {
                eprintln!(
                    "Warning: could not refresh from account ({err:#}); showing cached playlists."
                );
                (state.root.clone(), state.names.clone())
            }
            None => return Err(err),
        },
    };

    // Cached names fill gaps for playlists whose name could not be fetched.
    if let Some(state) = &cached {
        for (uri, name) in &state.names {
            names.entry(uri.clone()).or_insert_with(|| name.clone());
        }
    }

    let pre_checked: HashSet<String> = cached
        .as_ref()
        .map(|state| state.selected.iter().cloned().collect())
        .unwrap_or_default();
    let downloaded = cached
        .as_ref()
        .map(|state| state.downloaded.clone())
        .unwrap_or_default();

    let mut state = AccountState {
        root: root.clone(),
        names: names.clone(),
        selected: pre_checked.iter().cloned().collect(),
        downloaded: downloaded.clone(),
    };
    state.selected.sort();
    state.save()?;

    let handle = tokio::runtime::Handle::current();
    let mut refresh =
        move || tokio::task::block_in_place(|| handle.block_on(fetch_account_data(source, true)));

    let outcome = folder_picker::run(
        &root,
        names,
        pre_checked,
        downloaded,
        &mut |checked: &HashSet<String>| {
            state.selected = checked.iter().cloned().collect();
            state.selected.sort();
            if let Err(err) = state.save() {
                tracing::warn!("Could not save the selection: {err:#}");
            }
        },
        &mut refresh,
    )?;

    match outcome {
        Outcome::Selected(selected) => {
            state.selected = selected.clone();
            state.save()?;

            println!(
                "Downloading {} playlist{}:",
                selected.len(),
                if selected.len() == 1 { "" } else { "s" }
            );
            for playlist in &selected {
                println!("  - {playlist}");
            }

            Ok(selected)
        }
        Outcome::Cancelled => Err(anyhow!(
            "Aborted — the selection was saved and will be pre-selected on the next run"
        )),
    }
}

/// Marks the given playlists as downloaded in the persistent state.
pub fn mark_playlists_downloaded(uris: &[String]) -> Result<()> {
    AccountState::mark_downloaded(uris)
}

/// Fetches the folder tree and the playlist display names from the account.
pub async fn fetch_account_data(
    source: &AccountSource,
    quiet: bool,
) -> Result<(Folder, HashMap<String, String>)> {
    let uris = fetch_rootlist_uris(source).await?;
    let root = build_folder_tree(&uris);

    let mut all_uris = Vec::new();
    collect_all_uris(&root, &mut all_uris);
    if all_uris.is_empty() {
        return Err(anyhow!(
            "No playlists found in your account. \
             Pass playlist URIs or URLs as arguments instead."
        ));
    }

    let names = fetch_playlist_names(source, &dedupe(all_uris), quiet).await;
    Ok((root, names))
}

/// Fetches the account's rootlist, paginating until every entry has been read.
/// Returns the raw item URIs in sidebar order.
async fn fetch_rootlist_uris(source: &AccountSource) -> Result<Vec<String>> {
    let mut uris: Vec<String> = Vec::new();
    let mut from = 0usize;

    loop {
        let response = source.rootlist_page(from, ROOTLIST_PAGE_SIZE).await?;
        let content = SelectedListContent::parse_from_bytes(&response)
            .map_err(|err| anyhow!("Failed to parse the account's rootlist: {}", err))?;

        let total = content.length().max(0) as usize;
        let fetched = content
            .contents
            .get_or_default()
            .items
            .iter()
            .filter_map(|item| item.uri.clone())
            .collect::<Vec<_>>();

        let received = fetched.len();
        uris.extend(fetched);
        from += received;

        if received == 0 || from >= total || uris.len() >= MAX_ROOTLIST_ITEMS {
            break;
        }
    }

    Ok(uris)
}

/// The rootlist is a flat stream of playlist URIs interleaved with balanced
/// `spotify:start-group:<id>:<name>` / `spotify:end-group:<id>` folder markers;
/// nesting is recovered with a stack.
fn build_folder_tree(uris: &[String]) -> Folder {
    let mut root = Folder::default();
    let mut stack: Vec<Folder> = Vec::new();

    for uri in uris {
        match classify(uri) {
            RootlistItem::FolderStart(name) => stack.push(Folder {
                name: if name.trim().is_empty() {
                    "Unnamed folder".to_string()
                } else {
                    name
                },
                ..Folder::default()
            }),
            RootlistItem::FolderEnd => {
                if let Some(folder) = stack.pop() {
                    match stack.last_mut() {
                        Some(parent) => parent.children.push(folder),
                        None => root.children.push(folder),
                    }
                }
            }
            RootlistItem::Playlist(playlist) => match stack.last_mut() {
                Some(folder) => folder.playlists.push(playlist),
                None => root.playlists.push(playlist),
            },
            RootlistItem::Other => {}
        }
    }

    // Tolerate unbalanced markers (missing end markers) by flushing the
    // remaining open folders into the root, preserving their order.
    root.children.append(&mut stack);

    root
}

fn classify(uri: &str) -> RootlistItem {
    if let Some(rest) = uri.strip_prefix("spotify:start-group:") {
        let name = match rest.split_once(':') {
            Some((_, raw_name)) => decode_group_name(raw_name),
            None => String::new(),
        };
        RootlistItem::FolderStart(name)
    } else if uri.starts_with("spotify:end-group:") {
        RootlistItem::FolderEnd
    } else if let Some(index) = uri.find(":playlist:") {
        let id = &uri[index + ":playlist:".len()..];
        if SpotifyId::from_base62(id).is_ok() {
            RootlistItem::Playlist(uri.to_owned())
        } else {
            RootlistItem::Other
        }
    } else {
        RootlistItem::Other
    }
}

/// Folder names are carried in the start marker URI, form-urlencoded style:
/// `+` stands for a space and `%XX` escapes single bytes.
pub(crate) fn decode_group_name(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;

    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                decoded.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => match (hex_value(bytes[i + 1]), hex_value(bytes[i + 2]))
            {
                (Some(high), Some(low)) => {
                    decoded.push(high * 16 + low);
                    i += 3;
                }
                _ => {
                    decoded.push(b'%');
                    i += 1;
                }
            },
            byte => {
                decoded.push(byte);
                i += 1;
            }
        }
    }

    String::from_utf8_lossy(&decoded).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Collects every playlist URI in the tree, in tree order (with duplicates).
pub(crate) fn collect_all_uris(folder: &Folder, out: &mut Vec<String>) {
    out.extend(folder.playlists.iter().cloned());
    for child in &folder.children {
        collect_all_uris(child, out);
    }
}

/// Resolves the display names of the given playlists concurrently. Playlists
/// whose name cannot be fetched are simply absent from the result (the picker
/// falls back to showing their URI).
async fn fetch_playlist_names(
    source: &AccountSource,
    uris: &[String],
    quiet: bool,
) -> HashMap<String, String> {
    const CONCURRENCY: usize = 16;

    if uris.is_empty() {
        return HashMap::new();
    }

    let total = uris.len();
    let fetched = &AtomicUsize::new(0);
    if !quiet {
        eprint!("Fetching playlist names... 0/{total}");
    }

    let results = futures::stream::iter(uris.iter().map(|uri| async move {
        let name = playlist_name(source, uri).await;
        let done = fetched.fetch_add(1, Ordering::Relaxed) + 1;
        if !quiet {
            eprint!("\rFetching playlist names... {done}/{total}");
        }
        (uri.clone(), name)
    }))
    .buffer_unordered(CONCURRENCY)
    .collect::<Vec<_>>()
    .await;
    if !quiet {
        eprintln!();
    }

    let mut names = HashMap::with_capacity(results.len());
    for (uri, name) in results {
        if let Some(name) = name {
            names.insert(uri, name);
        }
    }
    names
}

async fn playlist_name(source: &AccountSource, uri: &str) -> Option<String> {
    let parsed = SpotifyUri::from_uri(uri).ok()?;
    let SpotifyUri::Playlist { id, .. } = parsed else {
        return None;
    };
    let content =
        SelectedListContent::parse_from_bytes(&source.playlist_bytes(&id).await.ok()?).ok()?;
    let name = content.attributes.get_or_default().name().trim();
    (!name.is_empty()).then(|| name.to_string())
}

fn dedupe(uris: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::with_capacity(uris.len());
    uris.into_iter()
        .filter(|uri| seen.insert(uri.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uris(items: &[&str]) -> Vec<String> {
        items.iter().map(|uri| uri.to_string()).collect()
    }

    #[test]
    fn decodes_folder_names() {
        assert_eq!(decode_group_name("Workout"), "Workout");
        assert_eq!(decode_group_name("Work+%26+Study"), "Work & Study");
        assert_eq!(decode_group_name("Deep%3Anested"), "Deep:nested");
        assert_eq!(decode_group_name("caf%C3%A9"), "café");
        assert_eq!(decode_group_name("trailing%2"), "trailing%2");
        assert_eq!(decode_group_name("bad%zz"), "bad%zz");
    }

    #[test]
    fn classifies_rootlist_items() {
        assert!(matches!(
            classify("spotify:start-group:edb339e10aebcf38:Workout"),
            RootlistItem::FolderStart(name) if name == "Workout"
        ));
        assert!(matches!(
            classify("spotify:end-group:edb339e10aebcf38"),
            RootlistItem::FolderEnd
        ));
        assert!(matches!(
            classify("spotify:playlist:37i9dQZF1DXcBWIGoYBM5M"),
            RootlistItem::Playlist(_)
        ));
        assert!(matches!(
            classify("spotify:user:foo:playlist:37i9dQZF1DXcBWIGoYBM5M"),
            RootlistItem::Playlist(_)
        ));
        // Invalid playlist ids and unrelated URIs are ignored.
        assert!(matches!(
            classify("spotify:playlist:not-base62"),
            RootlistItem::Other
        ));
        assert!(matches!(
            classify("spotify:collection"),
            RootlistItem::Other
        ));
        assert!(matches!(classify("spotify:track:xyz"), RootlistItem::Other));
    }

    #[test]
    fn builds_nested_folder_tree() {
        let root = build_folder_tree(&uris(&[
            "spotify:playlist:37i9dQZF1DXcBWIGoYBM5M",
            "spotify:start-group:aaaaaaaaaaaaaaaa:Work",
            "spotify:playlist:37i9dQZF1DX0XUsuxWHRQd",
            "spotify:start-group:bbbbbbbbbbbbbbbb:Gym",
            "spotify:playlist:37i9dQZF1DX4sWSpwq3LiO",
            "spotify:end-group:bbbbbbbbbbbbbbbb",
            "spotify:end-group:aaaaaaaaaaaaaaaa",
            "spotify:start-group:cccccccccccccccc:Empty",
            "spotify:end-group:cccccccccccccccc",
        ]));

        assert_eq!(
            root.playlists,
            uris(&["spotify:playlist:37i9dQZF1DXcBWIGoYBM5M"])
        );
        assert_eq!(root.children.len(), 2);

        let work = &root.children[0];
        assert_eq!(work.name, "Work");
        assert_eq!(
            work.playlists,
            uris(&["spotify:playlist:37i9dQZF1DX0XUsuxWHRQd"])
        );
        assert_eq!(work.children.len(), 1);
        assert_eq!(work.children[0].name, "Gym");
        assert_eq!(
            work.children[0].playlists,
            uris(&["spotify:playlist:37i9dQZF1DX4sWSpwq3LiO"])
        );

        assert_eq!(root.children[1].name, "Empty");
        assert!(root.children[1].playlists.is_empty());
    }

    #[test]
    fn unbalanced_markers_are_tolerated() {
        let root = build_folder_tree(&uris(&[
            "spotify:start-group:aaaaaaaaaaaaaaaa:Work",
            "spotify:playlist:37i9dQZF1DX0XUsuxWHRQd",
        ]));

        assert_eq!(root.children.len(), 1);
        assert_eq!(root.children[0].name, "Work");
        assert_eq!(
            root.children[0].playlists,
            uris(&["spotify:playlist:37i9dQZF1DX0XUsuxWHRQd"])
        );
    }
}
