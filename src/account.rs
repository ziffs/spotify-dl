use std::collections::HashSet;

use anyhow::Result;
use anyhow::anyhow;
use dialoguer::Select;
use dialoguer::theme::ColorfulTheme;
use librespot::core::session::Session;
use librespot::core::spotify_id::SpotifyId;
use librespot::protocol::playlist4_external::SelectedListContent;
use protobuf::Message;

/// Number of rootlist entries requested per page.
const ROOTLIST_PAGE_SIZE: usize = 500;
/// Hard cap on rootlist entries, so a misbehaving server cannot make us loop forever.
const MAX_ROOTLIST_ITEMS: usize = 10_000;

#[derive(Debug, Default)]
struct Folder {
    name: String,
    playlists: Vec<String>,
    children: Vec<Folder>,
}

/// A folder flattened out of the account's folder tree, with the playlists it
/// contains recursively (directly or through nested folders).
struct FolderEntry {
    name: String,
    depth: usize,
    playlists: Vec<String>,
}

enum RootlistItem {
    FolderStart(String),
    FolderEnd,
    Playlist(String),
    Other,
}

/// Queries the account's rootlist (the sidebar contents) and lets the user pick
/// one of its playlist folders interactively.
///
/// Returns the Spotify playlist URIs contained in the selected folder, so the
/// caller can proceed as if the user had passed them as download arguments.
pub async fn select_folder_playlists(session: &Session) -> Result<Vec<String>> {
    let uris = fetch_rootlist_uris(session).await?;
    let root = build_folder_tree(&uris);

    // Only actual folders are selectable; playlists left loose at the root of
    // the sidebar are out of scope here.
    let mut entries = Vec::new();
    for folder in &root.children {
        flatten_folders(folder, 0, &mut entries);
    }
    // Folders without any (recursive) playlist cannot be downloaded.
    entries.retain(|entry| !entry.playlists.is_empty());

    if entries.is_empty() {
        return Err(anyhow!(
            "No playlist folders found in your account. \
             Pass playlist URIs or URLs as arguments instead."
        ));
    }

    let items: Vec<String> = entries
        .iter()
        .map(|entry| {
            format!(
                "{}{} ({} playlist{})",
                "  ".repeat(entry.depth),
                entry.name,
                entry.playlists.len(),
                if entry.playlists.len() == 1 { "" } else { "s" }
            )
        })
        .collect();

    let selection = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("Select a playlist folder to download")
        .items(&items)
        .default(0)
        .interact_opt()
        .map_err(|err| anyhow!("Failed to read folder selection: {}", err))?;

    let Some(selection) = selection else {
        return Err(anyhow!("No folder selected"));
    };

    let selected = &entries[selection];
    println!(
        "Downloading {} playlist{} from folder \"{}\":",
        selected.playlists.len(),
        if selected.playlists.len() == 1 {
            ""
        } else {
            "s"
        },
        selected.name
    );
    for playlist in &selected.playlists {
        println!("  - {}", playlist);
    }

    Ok(selected.playlists.clone())
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

fn collect_playlists_recursive(folder: &Folder, out: &mut Vec<String>) {
    out.extend(folder.playlists.iter().cloned());
    for child in &folder.children {
        collect_playlists_recursive(child, out);
    }
}

fn flatten_folders(folder: &Folder, depth: usize, out: &mut Vec<FolderEntry>) {
    let mut playlists = Vec::new();
    collect_playlists_recursive(folder, &mut playlists);

    out.push(FolderEntry {
        name: folder.name.clone(),
        depth,
        playlists: dedupe(playlists),
    });

    for child in &folder.children {
        flatten_folders(child, depth + 1, out);
    }
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
    fn flattening_collects_playlists_recursively() {
        let root = build_folder_tree(&uris(&[
            "spotify:start-group:aaaaaaaaaaaaaaaa:Work",
            "spotify:playlist:37i9dQZF1DX0XUsuxWHRQd",
            "spotify:start-group:bbbbbbbbbbbbbbbb:Gym",
            "spotify:playlist:37i9dQZF1DX4sWSpwq3LiO",
            "spotify:end-group:bbbbbbbbbbbbbbbb",
            "spotify:end-group:aaaaaaaaaaaaaaaa",
        ]));

        let mut entries = Vec::new();
        for folder in &root.children {
            flatten_folders(folder, 0, &mut entries);
        }

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "Work");
        assert_eq!(entries[0].depth, 0);
        assert_eq!(
            entries[0].playlists,
            uris(&[
                "spotify:playlist:37i9dQZF1DX0XUsuxWHRQd",
                "spotify:playlist:37i9dQZF1DX4sWSpwq3LiO",
            ])
        );
        assert_eq!(entries[1].name, "Gym");
        assert_eq!(entries[1].depth, 1);
        assert_eq!(
            entries[1].playlists,
            uris(&["spotify:playlist:37i9dQZF1DX4sWSpwq3LiO"])
        );
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
