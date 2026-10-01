# 🎵 spotify-dl

A command line utility to download songs, podcasts, playlists and albums directly from Spotify.

> [!IMPORTANT]
> A Spotify Premium account is required.

> [!CAUTION]
> Usage of this software may infringe Spotify's terms of service or your local legislation. Use it under your own risk.

## 🚀 Features

- Download individual tracks, podcasts, playlists or full albums.
- Browse your account's playlist folders in an interactive tree view and pick individual playlists and/or whole folders to download.
- Remembers your selection, the folder tree, playlist names and what has already been downloaded between runs.
- Full-screen download view: live logs, per-playlist completion percentages, an overall progress bar and the remaining rate-limit budget (Ctrl-C aborts gracefully after the current song).
- Built with Rust for speed and efficiency.
- Supports metadata tagging and organized file output.

## ⚙️ Installation

You can install it from source or, if you're hacking on it, from a local checkout.

### From source

```
cargo install --git https://github.com/ziffs/spotify-dl.git
```

### From a local checkout

If you're hacking on spotify-dl, install from a local clone:

```
cargo install --path .
```

## 🧭 Usage

```
spotify-dl 0.9.2
A commandline utility to download music directly from Spotify

USAGE:
    spotify-dl [FLAGS] [OPTIONS] <tracks>...

FLAGS:
    -F, --force            Force download even if the file already exists
        --from-account     Browse the playlist folders of the logged-in account and pick one to download
    -h, --help             Prints help information
    -r, --no-rate-limit    Don't rate limit downloads (at most 30 downloads per 30 minutes)
    -V, --version          Prints version information

OPTIONS:
    -d, --destination <destination>    The directory where the songs will be downloaded
    -f, --format <format>              The format to download the tracks in. Default is mp3. [default: mp3]

ARGS:
    <tracks>...    A list of Spotify URIs or URLs (songs, podcasts, playlists or albums)
```

Songs, playlists and albums must be passed as Spotify URIs or URLs (e.g. `spotify:track:123456789abcdefghABCDEF` for songs and `spotify:playlist:123456789abcdefghABCDEF` for playlists or `https://open.spotify.com/playlist/123456789abcdefghABCDEF?si=1234567890`).

Alternatively, pass `--from-account` (instead of any track arguments) to browse the playlist folders of the logged-in account: spotify-dl queries the folders and the playlists they contain and shows a full-screen tree view where you can select/de-select individual playlists and/or whole folders (toggling a folder selects everything inside it, including nested folders). Confirming with Enter downloads every selected playlist as if you had passed them all as arguments.

The picker keeps its state in `~/.spotify-dl/account_state.json`: your selection is saved continuously and pre-selected on the next start, playlists already downloaded are marked with a ✓, and the folder tree and playlist names are cached so the picker also opens when Spotify cannot be reached. Press `R` inside the picker to refresh the metadata from your account.

During the download, a full-screen view shows the log output, every playlist with its completion percentage on the right, an overall progress bar and the remaining rate-limit budget. Songs that appear in several playlists advance all of their playlists at once, and `Ctrl-C` stops the run gracefully after the current song — the selection stays saved, so re-running `--from-account` and pressing Enter continues where you left off (files already on disk are skipped).

## 📁 Output structure

Tracks are saved as `Artist/Album/Title.ext` under the destination folder (default: current directory), where `ext` is the selected format (`flac` or `mp3`):

```
Music/
├── Daft Punk/
│   └── Discovery/
│       ├── One More Time.flac
│       └── Aerodynamic.flac
└── My Playlist.m3u
```

When downloading a playlist, a `<Playlist Name>.m3u` file is automatically created in the destination root, referencing the downloaded tracks with relative paths (e.g. `Daft Punk/Discovery/One More Time.flac`), so the folder can be moved as a unit.

## 📋 Examples

- Download a single track:

```bash
spotify-dl https://open.spotify.com/track/TRACK_ID
```

- Download a playlist:

```
spotify-dl -u YOUR_USER -p YOUR_PASS https://open.spotify.com/playlist/PLAYLIST_ID
```

Save as MP3 to a custom folder:

```
spotify-dl --format mp3 --destination ~/Music/Spotify https://open.spotify.com/album/ALBUM_ID
```

Download a playlist with rate limiting:

```
spotify-dl --rate-limit https://open.spotify.com/playlist/PLAYLIST_ID
```

Download playlists from your account (interactive tree view — pick playlists or whole folders):

```
spotify-dl --from-account
```

## 🧪 Captures & mocks (development)

Live runs can record every raw API response for offline testing:

```
SPOTIFY_DL_CAPTURE_DIR=/tmp/captures spotify-dl --from-account
```

The captures are anonymized into mock fixtures (every playlist id, folder id, name and username is replaced with deterministic random words & ids; playlist track lists are dropped) and used by the integration tests:

```
cargo run --example anonymize-captures -- /tmp/captures tests/fixtures/account
cargo test --test account_mock
```

The picker can also run entirely offline against those fixtures (`SPOTIFY_DL_MOCK_DIR`); confirming a selection then only reports what would have been downloaded:

```
SPOTIFY_DL_MOCK_DIR=tests/fixtures/account spotify-dl --from-account
```

## 📄 License

spotify-dl is licensed under the MIT license. See [LICENSE](LICENSE).
