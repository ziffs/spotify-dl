//! Capturing and mocking of the account (rootlist) API responses.
//!
//! Live runs can record every raw spclient response with [`CaptureStore`]
//! (enabled via the `SPOTIFY_DL_CAPTURE_DIR` environment variable). The
//! captures are anonymized with [`anonymize_capture_dir`] — every playlist id,
//! folder id, name and username is replaced with deterministic random data —
//! and written as fixtures that [`MockStore`] serves back, so the whole
//! `--from-account` flow can run offline and in integration tests.

use std::collections::HashMap;
use std::collections::HashSet;
use std::fs;
use std::io::ErrorKind;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use bytes::Bytes;
use librespot::core::spotify_id::SpotifyId;
use librespot::protocol::playlist4_external::ListAttributes;
use librespot::protocol::playlist4_external::SelectedListContent;
use protobuf::Message;
use protobuf::MessageField;

use crate::account::decode_group_name;

/// Page size used for rootlist requests (live, mock and fixtures alike).
pub const ROOTLIST_PAGE_SIZE: usize = 500;

/// Default seed for [`anonymize_capture_dir`]. A fixed seed keeps the
/// generated fixtures stable when the same captures are re-anonymized.
pub const DEFAULT_ANONYMIZE_SEED: u64 = 0x5EED_5EED_5EED_5EED;

fn rootlist_page_name(from: usize, length: usize) -> String {
    format!("rootlist_from{from}_len{length}.bin")
}

fn playlist_file_name(base62: &str) -> String {
    format!("playlist_{base62}.bin")
}

/// Records raw API responses of a live run to disk, keyed by request.
#[derive(Clone)]
pub struct CaptureStore {
    dir: PathBuf,
}

impl CaptureStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn save_rootlist_page(&self, from: usize, length: usize, response: &[u8]) -> Result<()> {
        self.save(&rootlist_page_name(from, length), response)
    }

    pub fn save_playlist(&self, id: &SpotifyId, response: &[u8]) -> Result<()> {
        self.save(&playlist_file_name(&id.to_base62()?), response)
    }

    fn save(&self, name: &str, data: &[u8]) -> Result<()> {
        fs::create_dir_all(&self.dir)?;
        fs::write(self.dir.join(name), data)
            .with_context(|| format!("writing capture {}", self.dir.join(name).display()))
    }
}

/// Serves previously captured (and anonymized) responses from a fixture
/// directory, through the same parsing path as live runs.
#[derive(Clone)]
pub struct MockStore {
    dir: PathBuf,
}

impl MockStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// The raw rootlist page for the given pagination window. Pages beyond the
    /// captured ones yield an empty response, which terminates the fetch loop.
    pub fn rootlist_page(&self, from: usize, length: usize) -> Result<Bytes> {
        let path = self.dir.join(rootlist_page_name(from, length));
        match fs::read(&path) {
            Ok(data) => Ok(Bytes::from(data)),
            Err(err) if err.kind() == ErrorKind::NotFound => Ok(Bytes::new()),
            Err(err) => {
                Err(err).with_context(|| format!("reading mock rootlist page {}", path.display()))
            }
        }
    }

    /// The raw playlist metadata response for the given playlist.
    pub fn playlist_bytes(&self, id: &SpotifyId) -> Result<Bytes> {
        let path = self.dir.join(playlist_file_name(&id.to_base62()?));
        let data = fs::read(&path).with_context(|| format!("no mock capture for playlist {id}"))?;
        Ok(Bytes::from(data))
    }

    /// The playlist URIs of a rootlist page (playlists and folder markers).
    pub fn rootlist_page_uris(&self, from: usize, length: usize) -> Result<Vec<String>> {
        let content = SelectedListContent::parse_from_bytes(&self.rootlist_page(from, length)?)?;
        Ok(content
            .contents
            .get_or_default()
            .items
            .iter()
            .filter_map(|item| item.uri.clone())
            .collect())
    }

    /// The playlist name stored in the fixture for the given base62 id, if a
    /// fixture exists.
    pub fn playlist_name(&self, base62: &str) -> Result<Option<String>> {
        let path = self.dir.join(playlist_file_name(base62));
        if !path.exists() {
            return Ok(None);
        }
        let content = SelectedListContent::parse_from_bytes(&fs::read(&path)?)?;
        Ok(Some(content.attributes.get_or_default().name().to_string()))
    }

    /// Walks all captured rootlist pages and returns the number of distinct
    /// playlists, folder start markers and folder end markers.
    pub fn rootlist_stats(&self) -> Result<(usize, usize, usize)> {
        let mut playlists: HashSet<String> = HashSet::new();
        let mut starts = 0usize;
        let mut ends = 0usize;
        let mut from = 0usize;
        loop {
            let uris = self.rootlist_page_uris(from, ROOTLIST_PAGE_SIZE)?;
            let received = uris.len();
            for uri in &uris {
                if uri.starts_with("spotify:playlist:") {
                    playlists.insert(uri.clone());
                } else if uri.starts_with("spotify:start-group:") {
                    starts += 1;
                } else if uri.starts_with("spotify:end-group:") {
                    ends += 1;
                }
            }
            from += received;
            if received == 0 {
                break;
            }
        }
        Ok((playlists.len(), starts, ends))
    }
}

/// Counts of what an anonymization pass produced.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct AnonymizeSummary {
    pub rootlist_pages: usize,
    pub playlists: usize,
    pub folders: usize,
}

/// Anonymizes a capture directory into a fixture directory.
///
/// Every playlist id, folder group id, playlist/folder name and username is
/// replaced with deterministic random data derived from `seed`; playlist
/// responses are trimmed to their metadata (track lists are dropped). The
/// original data is never written to the destination.
pub fn anonymize_capture_dir(
    src: impl AsRef<Path>,
    dst: impl AsRef<Path>,
    seed: u64,
) -> Result<AnonymizeSummary> {
    let src = src.as_ref();
    let dst = dst.as_ref();
    let mut rng = Rng::new(seed);
    let mut playlist_ids: HashMap<String, String> = HashMap::new();
    let mut group_ids: HashMap<String, String> = HashMap::new();
    let mut usernames: HashMap<String, String> = HashMap::new();
    let mut summary = AnonymizeSummary::default();

    fs::create_dir_all(dst)?;

    let mut entries: Vec<PathBuf> = fs::read_dir(src)?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .collect();
    entries.sort();

    for path in entries {
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !name.ends_with(".bin") {
            continue;
        }
        let data = fs::read(&path)?;
        let mut content = SelectedListContent::parse_from_bytes(&data)
            .with_context(|| format!("parsing capture {name}"))?;

        if let Some(original_id) = name
            .strip_prefix("playlist_")
            .and_then(|n| n.strip_suffix(".bin"))
        {
            let new_id = playlist_ids
                .entry(original_id.to_string())
                .or_insert_with(|| random_base62(&mut rng));
            anonymize_playlist(&mut content, &mut usernames, &mut rng)?;
            fs::write(
                dst.join(playlist_file_name(new_id)),
                content.write_to_bytes()?,
            )?;
            summary.playlists += 1;
        } else if name.starts_with("rootlist_") {
            anonymize_rootlist(
                &mut content,
                &mut playlist_ids,
                &mut group_ids,
                &mut usernames,
                &mut rng,
                &mut summary,
            )?;
            fs::write(dst.join(name), content.write_to_bytes()?)?;
            summary.rootlist_pages += 1;
        }
    }

    Ok(summary)
}

fn anonymize_rootlist(
    content: &mut SelectedListContent,
    playlist_ids: &mut HashMap<String, String>,
    group_ids: &mut HashMap<String, String>,
    usernames: &mut HashMap<String, String>,
    rng: &mut Rng,
    summary: &mut AnonymizeSummary,
) -> Result<()> {
    content.revision = Some(random_bytes(rng, 16));
    if let Some(mut attributes) = content.attributes.take() {
        anonymize_list_attributes(&mut attributes, rng);
        content.attributes = MessageField::from_option(Some(attributes));
    }
    content.owner_username = Some(anonymize_username(
        content.owner_username.take(),
        usernames,
        rng,
    ));

    let mut contents = content.contents.take().unwrap_or_default();
    for item in &mut contents.items {
        if let Some(uri) = item.uri.take() {
            item.uri = Some(anonymize_uri(&uri, playlist_ids, group_ids, rng, summary)?);
        }
        item.attributes = MessageField::none();
    }
    contents.meta_items.clear();
    contents.available_signals.clear();
    contents.continuation_token = None;
    content.contents = MessageField::from_option(Some(contents));

    clear_response_extras(content);
    Ok(())
}

fn anonymize_playlist(
    content: &mut SelectedListContent,
    usernames: &mut HashMap<String, String>,
    rng: &mut Rng,
) -> Result<()> {
    content.revision = Some(random_bytes(rng, 16));
    if let Some(mut attributes) = content.attributes.take() {
        anonymize_list_attributes(&mut attributes, rng);
        // Playlists get a slightly longer name than the rootlist itself.
        let word_count = 2 + rng.gen_range(2);
        attributes.name = Some(random_words(rng, word_count));
        content.attributes = MessageField::from_option(Some(attributes));
    }
    content.owner_username = Some(anonymize_username(
        content.owner_username.take(),
        usernames,
        rng,
    ));
    // Track lists are dropped: fixtures only carry playlist metadata.
    content.contents = MessageField::none();

    clear_response_extras(content);
    Ok(())
}

fn anonymize_list_attributes(attributes: &mut ListAttributes, rng: &mut Rng) {
    attributes.name = Some(random_words(rng, 2));
    attributes.description = None;
    attributes.picture = None;
    attributes.pl3_version = None;
    attributes.client_id = None;
    attributes.format = None;
    attributes.format_attributes.clear();
    attributes.picture_size.clear();
    attributes.sequence_context_template = None;
    attributes.ai_curation_reference_id = None;
}

fn clear_response_extras(content: &mut SelectedListContent) {
    content.diff = MessageField::none();
    content.sync_result = MessageField::none();
    content.capabilities = MessageField::none();
    content.applied_lenses = MessageField::none();
    content.resulting_revisions.clear();
    content.nonces.clear();
    content.geoblock.clear();
}

fn anonymize_uri(
    uri: &str,
    playlist_ids: &mut HashMap<String, String>,
    group_ids: &mut HashMap<String, String>,
    rng: &mut Rng,
    summary: &mut AnonymizeSummary,
) -> Result<String> {
    if let Some(id) = uri.strip_prefix("spotify:playlist:") {
        let new_id = playlist_ids
            .entry(id.to_string())
            .or_insert_with(|| random_base62(rng));
        Ok(format!("spotify:playlist:{new_id}"))
    } else if let Some(rest) = uri.strip_prefix("spotify:start-group:") {
        let (hex, encoded_name) = rest.split_once(':').unwrap_or((rest, ""));
        let new_hex = group_ids
            .entry(hex.to_string())
            .or_insert_with(|| random_hex(rng));
        // The folder name is decoded only to validate it; the replacement is
        // freshly generated random words in the same wire format.
        let _decoded = decode_group_name(encoded_name);
        summary.folders += 1;
        Ok(format!(
            "spotify:start-group:{new_hex}:{}",
            encode_group_name(&random_words(rng, 2))
        ))
    } else if let Some(hex) = uri.strip_prefix("spotify:end-group:") {
        let new_hex = group_ids
            .entry(hex.to_string())
            .or_insert_with(|| random_hex(rng));
        Ok(format!("spotify:end-group:{new_hex}"))
    } else {
        Err(anyhow!("unexpected rootlist item uri: {uri}"))
    }
}

fn anonymize_username(
    original: Option<String>,
    usernames: &mut HashMap<String, String>,
    rng: &mut Rng,
) -> String {
    let key = original.unwrap_or_default();
    usernames
        .entry(key)
        .or_insert_with(|| random_word(rng).to_string())
        .clone()
}

/// Encodes a folder name into the start-marker wire format (`+` for spaces,
/// percent-escapes otherwise), mirroring `decode_group_name`.
fn encode_group_name(name: &str) -> String {
    let mut out = String::new();
    for byte in name.bytes() {
        match byte {
            b' ' => out.push('+'),
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char);
            }
            _ => {
                out.push('%');
                out.push_str(&format!("{byte:02X}"));
            }
        }
    }
    out
}

const WORDS: [&str; 60] = [
    "amber", "anchor", "basin", "breeze", "canyon", "cedar", "comet", "coral", "crimson", "dawn",
    "delta", "drift", "ember", "falcon", "fern", "flint", "forge", "glade", "granite", "harbor",
    "hazel", "iris", "ivory", "jade", "juniper", "lagoon", "lantern", "lichen", "maple", "marble",
    "meadow", "mesa", "mint", "north", "oasis", "onyx", "opal", "orchid", "pine", "prairie",
    "quartz", "raven", "reef", "ridge", "sage", "shore", "slate", "spruce", "storm", "summit",
    "tide", "timber", "topaz", "tundra", "valley", "velvet", "vertex", "willow", "winter",
    "zenith",
];

const HEX: &[u8; 16] = b"0123456789abcdef";

/// Small xorshift64* generator; enough for deterministic test fixtures.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn gen_range(&mut self, bound: usize) -> usize {
        (self.next_u64() % bound as u64) as usize
    }
}

fn random_word(rng: &mut Rng) -> &'static str {
    WORDS[rng.gen_range(WORDS.len())]
}

fn random_words(rng: &mut Rng, count: usize) -> String {
    let mut picked: HashSet<&'static str> = HashSet::with_capacity(count);
    let mut words = Vec::with_capacity(count);
    while words.len() < count {
        let word = random_word(rng);
        if picked.insert(word) {
            words.push(word);
        }
    }
    words.join(" ")
}

fn random_base62(rng: &mut Rng) -> String {
    // Real Spotify ids are the base62 encoding of a 128-bit number, so the
    // random id is generated as 16 random bytes and encoded the same way.
    let bytes = random_bytes(rng, 16);
    SpotifyId::from_raw(&bytes)
        .expect("16 bytes always form a valid SpotifyId")
        .to_base62()
        .expect("base62 encoding of a 128-bit id is always 22 chars")
}

fn random_hex(rng: &mut Rng) -> String {
    (0..16)
        .map(|_| HEX[rng.gen_range(HEX.len())] as char)
        .collect()
}

fn random_bytes(rng: &mut Rng, len: usize) -> Vec<u8> {
    (0..len).map(|_| (rng.next_u64() & 0xFF) as u8).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use librespot::protocol::playlist4_external::Item;
    use librespot::protocol::playlist4_external::ListItems;
    use tempfile::tempdir;

    fn item(uri: &str) -> Item {
        Item {
            uri: Some(uri.to_string()),
            ..Default::default()
        }
    }

    fn synthetic_rootlist() -> SelectedListContent {
        let mut content = SelectedListContent::default();
        content.length = Some(4);
        content.owner_username = Some("claus92".to_string());
        let mut items = ListItems::default();
        items.pos = Some(0);
        items.truncated = Some(false);
        items.items = vec![
            item("spotify:playlist:AAAAAAAAAAAAAAAAAAAAAA"),
            item("spotify:start-group:deadbeefdeadbeef:My+Secret+Folder"),
            item("spotify:playlist:BBBBBBBBBBBBBBBBBBBBBB"),
            item("spotify:end-group:deadbeefdeadbeef"),
        ];
        content.contents = MessageField::from_option(Some(items));
        content
    }

    fn synthetic_playlist() -> SelectedListContent {
        let mut content = SelectedListContent::default();
        content.length = Some(42);
        content.owner_username = Some("claus92".to_string());
        let mut attributes = ListAttributes::default();
        attributes.name = Some("My Secret Playlist".to_string());
        attributes.description = Some("a very secret description".to_string());
        content.attributes = MessageField::from_option(Some(attributes));
        content
    }

    fn write_captures(dir: &Path) {
        fs::write(
            dir.join("rootlist_from0_len500.bin"),
            synthetic_rootlist().write_to_bytes().unwrap(),
        )
        .unwrap();
        fs::write(
            dir.join("playlist_AAAAAAAAAAAAAAAAAAAAAA.bin"),
            synthetic_playlist().write_to_bytes().unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn anonymize_replaces_ids_names_and_users() {
        let src = tempdir().unwrap();
        let dst = tempdir().unwrap();
        write_captures(src.path());

        let summary =
            anonymize_capture_dir(src.path(), dst.path(), DEFAULT_ANONYMIZE_SEED).unwrap();
        assert_eq!(summary.rootlist_pages, 1);
        assert_eq!(summary.playlists, 1);
        assert_eq!(summary.folders, 1);

        let rootlist = SelectedListContent::parse_from_bytes(
            &fs::read(dst.path().join("rootlist_from0_len500.bin")).unwrap(),
        )
        .unwrap();
        let uris: Vec<String> = rootlist
            .contents
            .get_or_default()
            .items
            .iter()
            .filter_map(|item| item.uri.clone())
            .collect();
        assert_eq!(uris.len(), 4);

        // The playlist id is replaced with a fresh, valid base62 id.
        let new_playlist = uris[0].strip_prefix("spotify:playlist:").unwrap();
        assert_ne!(new_playlist, "AAAAAAAAAAAAAAAAAAAAAA");
        assert_eq!(new_playlist.len(), 22);
        assert!(SpotifyId::from_base62(new_playlist).is_ok());

        // Paired folder markers share the new group id; the name is decoded
        // random words in the same wire format.
        let start = uris[1].strip_prefix("spotify:start-group:").unwrap();
        let (new_hex, encoded_name) = start.split_once(':').unwrap();
        assert_ne!(new_hex, "deadbeefdeadbeef");
        assert_eq!(new_hex.len(), 16);
        let end = uris[3].strip_prefix("spotify:end-group:").unwrap();
        assert_eq!(end, new_hex);
        let decoded = decode_group_name(encoded_name);
        assert_ne!(decoded, "My Secret Folder");
        assert!(decoded.contains(' '));
        assert!(!decoded.contains('%') && !decoded.contains('+'));

        // The username is replaced.
        assert_ne!(rootlist.owner_username.as_deref(), Some("claus92"));

        // The playlist fixture is keyed by the new id, carries a random name
        // and no longer contains the track list.
        let playlist = SelectedListContent::parse_from_bytes(
            &fs::read(dst.path().join(playlist_file_name(new_playlist))).unwrap(),
        )
        .unwrap();
        let name = playlist.attributes.get_or_default().name().to_string();
        assert_ne!(name, "My Secret Playlist");
        assert!(!name.is_empty());
        assert!(playlist.contents.is_none());

        // No original data survives anywhere in the fixtures.
        for entry in fs::read_dir(dst.path()).unwrap() {
            let data = fs::read(entry.unwrap().path()).unwrap();
            let text = String::from_utf8_lossy(&data);
            assert!(!text.contains("AAAAAAAAAAAAAAAAAAAAAA"), "{text}");
            assert!(!text.contains("BBBBBBBBBBBBBBBBBBBBBB"), "{text}");
            assert!(!text.contains("deadbeefdeadbeef"), "{text}");
            assert!(!text.contains("claus92"), "{text}");
            assert!(!text.contains("My Secret"), "{text}");
        }
    }

    #[test]
    fn anonymize_is_deterministic() {
        let src = tempdir().unwrap();
        let dst_a = tempdir().unwrap();
        let dst_b = tempdir().unwrap();
        write_captures(src.path());

        anonymize_capture_dir(src.path(), dst_a.path(), DEFAULT_ANONYMIZE_SEED).unwrap();
        anonymize_capture_dir(src.path(), dst_b.path(), DEFAULT_ANONYMIZE_SEED).unwrap();

        let mut names: Vec<String> = fs::read_dir(dst_a.path())
            .unwrap()
            .filter_map(|entry| {
                entry
                    .ok()
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
            })
            .collect();
        names.sort();
        assert!(!names.is_empty());
        for name in names {
            assert_eq!(
                fs::read(dst_a.path().join(&name)).unwrap(),
                fs::read(dst_b.path().join(&name)).unwrap(),
                "fixture {name} differs between runs"
            );
        }
    }

    #[test]
    fn mock_store_serves_fixtures_and_paginates() {
        let src = tempdir().unwrap();
        let dst = tempdir().unwrap();
        write_captures(src.path());
        anonymize_capture_dir(src.path(), dst.path(), DEFAULT_ANONYMIZE_SEED).unwrap();

        let store = MockStore::new(dst.path());
        let uris = store.rootlist_page_uris(0, ROOTLIST_PAGE_SIZE).unwrap();
        assert_eq!(uris.len(), 4);

        // Pages beyond the captured ones terminate pagination.
        let beyond = store
            .rootlist_page_uris(ROOTLIST_PAGE_SIZE, ROOTLIST_PAGE_SIZE)
            .unwrap();
        assert!(beyond.is_empty());

        let (playlists, starts, ends) = store.rootlist_stats().unwrap();
        assert_eq!(playlists, 2);
        assert_eq!(starts, 1);
        assert_eq!(ends, 1);

        let new_playlist = uris[0].strip_prefix("spotify:playlist:").unwrap();
        let name = store.playlist_name(new_playlist).unwrap().unwrap();
        assert!(!name.is_empty());
        // The original id has no fixture under its own name.
        assert!(
            store
                .playlist_name("AAAAAAAAAAAAAAAAAAAAAA")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn encode_group_name_mirrors_decode() {
        for name in [
            "simple",
            "two words",
            "weird & stuff!",
            "café ünïcode",
            "100% done",
        ] {
            let encoded = encode_group_name(name);
            assert_eq!(
                decode_group_name(&encoded),
                name,
                "roundtrip failed for {name}"
            );
        }
    }
}
