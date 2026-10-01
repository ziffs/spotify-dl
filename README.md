# 🎵 spotify-dl

A command line utility to download songs, podcasts, playlists and albums directly from Spotify.

> [!IMPORTANT]
> A Spotify Premium account is required.

> [!CAUTION]
> Usage of this software may infringe Spotify's terms of service or your local legislation. Use it under your own risk.

## 🚀 Features

- Download individual tracks, podcasts, playlists or full albums.
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

## 📄 License

spotify-dl is licensed under the MIT license. See [LICENSE](LICENSE).
