//! Integration tests for the `--from-account` flow, running against mock
//! fixtures captured from a live account and anonymized (random words & ids).
//!
//! The fixtures live in `tests/fixtures/account`; they can be regenerated from
//! a capture directory with
//! `cargo run --example anonymize-captures -- <capture-dir> tests/fixtures/account`.

use std::collections::HashSet;
use std::path::PathBuf;

use spotify_dl::account::AccountSource;
use spotify_dl::account::Folder;
use spotify_dl::account::fetch_account_data;
use spotify_dl::capture::MockStore;
use spotify_dl::capture::ROOTLIST_PAGE_SIZE;
use spotify_dl::folder_picker::FolderPicker;
use spotify_dl::folder_picker::RowKind;

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/account")
}

fn collect_tree_uris(folder: &Folder, out: &mut Vec<String>) {
    out.extend(folder.playlists.iter().cloned());
    for child in &folder.children {
        collect_tree_uris(child, out);
    }
}

fn collect_folder_names(folder: &Folder, out: &mut Vec<String>) {
    for child in &folder.children {
        out.push(child.name.clone());
        collect_folder_names(child, out);
    }
}

/// The recursive playlists of a folder, in tree order (mirrors the picker).
fn collect_recursive(folder: &Folder, out: &mut Vec<String>) {
    out.extend(folder.playlists.iter().cloned());
    for child in &folder.children {
        collect_recursive(child, out);
    }
}

#[tokio::test]
async fn builds_tree_and_names_from_mock_account() {
    let source = AccountSource::mock(fixtures_dir());
    let (root, names) = fetch_account_data(&source, true)
        .await
        .expect("fetching account data from mock fixtures");

    let store = MockStore::new(fixtures_dir());
    let (playlists, starts, ends) = store.rootlist_stats().unwrap();
    assert!(playlists > 0, "fixtures must contain playlists");
    assert!(starts > 0, "fixtures must contain folders");
    assert_eq!(starts, ends, "folder markers must be balanced");

    // The tree contains exactly the playlists of the captured rootlist.
    let mut tree_uris = Vec::new();
    collect_tree_uris(&root, &mut tree_uris);
    let tree_set: HashSet<&String> = tree_uris.iter().collect();
    assert_eq!(tree_set.len(), playlists, "distinct tree playlists");
    assert_eq!(tree_uris.len(), playlists, "no duplicated playlists");

    // Folder markers became folders with decoded (random word) names.
    let mut folder_names = Vec::new();
    collect_folder_names(&root, &mut folder_names);
    assert_eq!(folder_names.len(), starts, "one folder per start marker");
    for name in &folder_names {
        assert!(!name.is_empty(), "folder names must not be empty");
        assert!(
            !name.contains('+') && !name.contains('%'),
            "decoded name: {name}"
        );
    }

    // Every playlist resolved a display name from its fixture.
    assert_eq!(names.len(), playlists, "one name per playlist");
    for (uri, name) in &names {
        assert!(
            !name.is_empty() && !name.contains('%'),
            "name for {uri}: {name}"
        );
        let base62 = uri.strip_prefix("spotify:playlist:").unwrap();
        assert_eq!(
            Some(name.clone()),
            store.playlist_name(base62).unwrap(),
            "flow name must match the fixture name for {uri}"
        );
    }
}

#[tokio::test]
async fn mock_pagination_serves_captured_pages_then_stops() {
    let store = MockStore::new(fixtures_dir());

    let mut total = 0usize;
    let mut from = 0usize;
    loop {
        let uris = store
            .rootlist_page_uris(from, ROOTLIST_PAGE_SIZE)
            .expect("rootlist page");
        let received = uris.len();
        total += received;
        from += received;
        if received == 0 {
            break;
        }
    }
    assert!(total > 0, "the first captured page must have items");

    // A page far beyond the captured range is empty.
    let beyond = store
        .rootlist_page_uris(100_000, ROOTLIST_PAGE_SIZE)
        .unwrap();
    assert!(beyond.is_empty());
}

#[tokio::test]
async fn picker_selects_whole_folder_from_mock_tree() {
    let source = AccountSource::mock(fixtures_dir());
    let (root, names) = fetch_account_data(&source, true).await.unwrap();

    let mut picker = FolderPicker::new(&root, names);
    assert!(picker.row_count() > 0);

    // The first folder row sits right after the root-level loose playlists.
    let folder_row = root.playlists.len();
    assert_eq!(picker.row_kind(folder_row), Some(RowKind::Folder));

    picker.set_cursor(folder_row);
    picker.toggle_current();

    // Selecting the folder selects every playlist inside it, recursively.
    let mut expected = Vec::new();
    collect_recursive(&root.children[0], &mut expected);
    assert_eq!(picker.selected_uris(), expected);
    assert_eq!(picker.selected_count(), expected.len());

    // Toggling again clears the folder.
    picker.toggle_current();
    assert_eq!(picker.selected_count(), 0);
}

#[tokio::test]
async fn picker_pre_selects_saved_playlists_from_mock_tree() {
    let source = AccountSource::mock(fixtures_dir());
    let (root, names) = fetch_account_data(&source, true).await.unwrap();

    let mut tree_uris = Vec::new();
    collect_tree_uris(&root, &mut tree_uris);
    let saved: Vec<String> = tree_uris.iter().take(3).cloned().collect();

    let picker =
        FolderPicker::new(&root, names.clone()).with_pre_checked(saved.iter().cloned().collect());
    assert_eq!(picker.selected_count(), 3);
    assert_eq!(picker.selected_uris(), saved);

    // A saved playlist that no longer exists in the tree is dropped.
    let picker = FolderPicker::new(&root, names.clone()).with_pre_checked(HashSet::from([
        "spotify:playlist:0000000000000000000000".to_string(),
    ]));
    assert_eq!(picker.selected_count(), 0);
}
