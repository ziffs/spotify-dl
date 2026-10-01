use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use anyhow::Result;
use anyhow::anyhow;
use futures::StreamExt;
use librespot::core::SpotifyUri;
use librespot::core::session::Session;
use librespot::core::spotify_id::SpotifyId;
use librespot::metadata::Metadata;
use librespot::metadata::Playlist;
use librespot::protocol::playlist4_external::SelectedListContent;
use protobuf::Message;

use crate::folder_picker;

/// Number of rootlist entries requested per page.
const ROOTLIST_PAGE_SIZE: usize = 500;
/// Hard cap on rootlist entries, so a misbehaving server cannot make us loop forever.
const MAX_ROOTLIST_ITEMS: usize = 10_000;

#[derive(Debug, Default)]
pub(crate) struct Folder {
    pub(crate) name: String,
    pub(crate) playlists: Vec<String>,
    pub(crate) children: Vec<Folder>,
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
pub async fn select_folder_playlists(session: &Session) -> Result<Vec<String>> {
    let uris = fetch_rootlist_uris(session).await?;
    let root = build_folder_tree(&uris);

    let mut all_uris = Vec::new();
    collect_all_uris(&root, &mut all_uris);
    if all_uris.is_empty() {
        return Err(anyhow!(
            "No playlists found in your account. \
             Pass playlist URIs or URLs as arguments instead."
        ));
    }

    let names = fetch_playlist_names(session, &dedupe(all_uris)).await;

    let selected = folder_picker::run(&root, names)?;

    println!(
        "Downloading {} playlist{}:",
        selected.len(),
        if selected.len() == 1 { "" } else { "s" }
    );
    for playlist in &selected {
        println!("  - {}", playlist);
    }

    Ok(selected)
}

/// Fetches the account's rootlist, paginating until every entry has been read.
/// Returns the raw item URIs in sidebar order.
async fn fetch_rootlist_uris(session: &Session) -> Result<Vec<String>> {
    let spclient = session.spclient();
    let mut uris: Vec<String> = Vec::new();
    let mut from = 0usize;

    loop {
        let response = spclient
            .get_rootlist(from, Some(ROOTLIST_PAGE_SIZE))
            .await?;
        let content = SelectedListContent::parse_from_bytes(&response)
            .map_err(|err| anyhow!("Failed to parse the account's rootlist: {}", err))?;

        let total = content.length().max(0) as usize;
        let fetched = content
            .contents
            .get_or_default()
            .items
            .iter()
            .map(|item| item.uri().to_owned())
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
fn decode_group_name(raw: &str) -> String {
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
fn collect_all_uris(folder: &Folder, out: &mut Vec<String>) {
    out.extend(folder.playlists.iter().cloned());
    for child in &folder.children {
        collect_all_uris(child, out);
    }
}

/// Resolves the display names of the given playlists concurrently. Playlists
/// whose name cannot be fetched are simply absent from the result (the picker
/// falls back to showing their URI).
async fn fetch_playlist_names(session: &Session, uris: &[String]) -> HashMap<String, String> {
    const CONCURRENCY: usize = 16;

    if uris.is_empty() {
        return HashMap::new();
    }

    let total = uris.len();
    let fetched = &AtomicUsize::new(0);
    eprint!("Fetching playlist names... 0/{total}");

    let results = futures::stream::iter(uris.iter().map(|uri| {
        let session = session.clone();
        async move {
            let name = playlist_name(&session, uri).await;
            let done = fetched.fetch_add(1, Ordering::Relaxed) + 1;
            eprint!("\rFetching playlist names... {done}/{total}");
            (uri.clone(), name)
        }
    }))
    .buffer_unordered(CONCURRENCY)
    .collect::<Vec<_>>()
    .await;
    eprintln!();

    let mut names = HashMap::with_capacity(results.len());
    for (uri, name) in results {
        if let Some(name) = name {
            names.insert(uri, name);
        }
    }
    names
}

async fn playlist_name(session: &Session, uri: &str) -> Option<String> {
    let parsed = SpotifyUri::from_uri(uri).ok()?;
    let playlist = Playlist::get(session, &parsed).await.ok()?;
    let name = playlist.attributes.name.trim();
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
