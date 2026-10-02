use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use librespot::core::SpotifyUri;
use librespot::core::session::Session;

use crate::download_ui::DownloadUi;
use crate::encoder;
use crate::encoder::Format;
use crate::encoder::Samples;
use crate::rate_limit::PersistentRateLimiter;
use crate::stream::Stream;
use crate::stream::StreamError;
use crate::stream::StreamEvent;
use crate::stream::StreamEventChannel;
use crate::track::Track;
use crate::track::TrackMetadata;

/// A failure while streaming a song, with enough context for the playlist
/// comment and the unavailable-tracks file.
struct TrackFailure {
    error: String,
    unavailable: bool,
}

/// The result of processing a single song.
enum TrackOutcome {
    /// The file was written by this run; carries the relative path and the
    /// size on disk.
    Downloaded { path: String, bytes: u64 },
    /// The file already existed; carries the relative path.
    Skipped { path: String },
    /// Nothing on disk; the song is recorded as a comment in the playlist
    /// files that contain it.
    Failed {
        error: String,
        /// Display name of the song, when its metadata was known.
        name: Option<String>,
        /// Where the song would have been saved, when its metadata was known.
        path: Option<String>,
        /// Spotify reported the song as unavailable (never retried).
        unavailable: bool,
    },
}

pub struct Downloader {
    session: Session,
}

#[derive(Debug, Clone)]
pub struct DownloadOptions {
    pub destination: PathBuf,
    pub format: Format,
    pub force: bool,
    pub rate_limit: bool,
    pub rate_limiter: Option<Arc<PersistentRateLimiter>>,
}

impl DownloadOptions {
    pub fn new(
        destination: Option<String>,
        format: Format,
        force: bool,
        rate_limit: bool,
        rate_limiter: Option<Arc<PersistentRateLimiter>>,
    ) -> Self {
        let destination =
            destination.map_or_else(|| std::env::current_dir().unwrap(), PathBuf::from);
        DownloadOptions {
            destination,
            format,
            force,
            rate_limit,
            rate_limiter,
        }
    }
}

impl Downloader {
    pub fn new(session: Session) -> Self {
        Downloader { session }
    }

    /// Downloads the given tracks, updating `ui` with progress.
    ///
    /// Songs appearing in several playlists are downloaded once; every
    /// playlist that contains a finished song gets its playlist file updated
    /// and its percentage advanced. `Ctrl-C` in the download view requests a
    /// graceful abort between songs.
    pub async fn download_tracks(
        self,
        tracks: Vec<Track>,
        options: &DownloadOptions,
        ui: &DownloadUi,
    ) -> Result<()> {
        if options.rate_limit {
            tracing::info!("Rate limiting enabled: at most 30 downloads per 30 minutes");
        }

        let mut seen: HashSet<SpotifyUri> = HashSet::new();
        let mut playlist_files: Vec<(String, Vec<String>)> = Vec::new();
        for track in tracks.into_iter() {
            if ui.is_aborted() {
                ui.finish();
                return Err(anyhow::anyhow!("Download aborted"));
            }
            // The same song can appear in several playlists (or twice in one);
            // it is downloaded once and every playlist containing it is
            // updated when it finishes.
            if !seen.insert(track.id.clone()) {
                continue;
            }

            ui.track_started(&track);
            let outcome = match self.download_track(&track, options, ui).await {
                Ok(outcome) => outcome,
                Err(err) => {
                    tracing::warn!("Error in track {:?}: {:?}", track.id, err);
                    TrackOutcome::Failed {
                        error: format!("{err:#}"),
                        name: None,
                        path: None,
                        unavailable: false,
                    }
                }
            };

            match &outcome {
                TrackOutcome::Downloaded { bytes, .. } => {
                    ui.track_completed(&track.id, true, *bytes)
                }
                TrackOutcome::Skipped { .. } => ui.track_completed(&track.id, false, 0),
                TrackOutcome::Failed {
                    error,
                    name,
                    unavailable,
                    ..
                } => {
                    if *unavailable
                        && let Err(err) =
                            crate::unavailable::record(&track.id, name.as_deref(), error)
                    {
                        tracing::warn!("Could not update the unavailable tracks file: {err:#}");
                    }
                }
            }

            // The playlist entry for this song: its path, or a comment that
            // keeps failed songs visible instead of silently dropping them.
            let m3u_entry = match &outcome {
                TrackOutcome::Downloaded { path, .. } | TrackOutcome::Skipped { path } => {
                    path.clone()
                }
                TrackOutcome::Failed { error, path, .. } => {
                    Self::failed_m3u_entry(error, path.as_deref(), &track.id)
                }
            };

            for playlist in ui.playlists_of(&track.id) {
                let index = match playlist_files
                    .iter()
                    .position(|(name, _)| name == &playlist)
                {
                    Some(index) => index,
                    None => {
                        playlist_files.push((playlist.clone(), Vec::new()));
                        playlist_files.len() - 1
                    }
                };
                playlist_files[index].1.push(m3u_entry.clone());

                Self::write_playlist_file(
                    &options.destination,
                    &playlist,
                    &playlist_files[index].1,
                )
                .await?;
                ui.playlist_synced(&playlist);
            }
        }
        ui.finish();
        Ok(())
    }

    async fn write_playlist_file(
        destination: &Path,
        playlist: &str,
        entries: &[String],
    ) -> Result<()> {
        let mut content = String::from("#EXTM3U\n");
        for entry in entries {
            content.push_str(entry);
            content.push('\n');
        }
        let playlist_file = destination.join(format!("{}.m3u", playlist));
        tokio::fs::write(&playlist_file, content).await?;
        tracing::info!(
            "Updated playlist file: {:?} ({} entries)",
            playlist_file,
            entries.len()
        );
        Ok(())
    }

    #[tracing::instrument(
        name = "download_track",
        skip(self, track, options, ui),
        fields(track = %track.id)
    )]
    async fn download_track(
        &self,
        track: &Track,
        options: &DownloadOptions,
        ui: &DownloadUi,
    ) -> Result<TrackOutcome> {
        let metadata = track.metadata(&self.session).await;
        tracing::info!("Downloading track: {:?}", metadata.track_name);
        ui.track_metadata(metadata.to_string(), metadata.approx_size() as u64);

        let relative_path = format!(
            "{}.{}",
            metadata.to_path_string(),
            options.format.extension()
        );
        let path = options
            .destination
            .join(&relative_path)
            .to_str()
            .ok_or(anyhow::anyhow!("Could not set the output path"))?
            .to_string();

        if !options.force && PathBuf::from(&path).exists() {
            tracing::info!(
                "Skipping {}, file already exists. Use --force to force re-downloading the track",
                &metadata.track_name
            );
            return Ok(TrackOutcome::Skipped {
                path: relative_path,
            });
        }

        // Only actual downloads are rate limited, not metadata requests or
        // tracks skipped because they already exist on disk.
        if options.rate_limit {
            if let Some(rate_limiter) = &options.rate_limiter {
                rate_limiter.acquire().await?;
            }
        }

        let stream = Stream::new(self.session.clone());
        let channel = match stream.stream(track).await {
            Ok(channel) => channel,
            Err(err) => {
                tracing::error!("Failed to download {}: {}", metadata.to_string(), err);
                return Ok(TrackOutcome::Failed {
                    error: format!("could not start streaming: {err:#}"),
                    name: Some(metadata.to_string()),
                    path: Some(relative_path),
                    unavailable: false,
                });
            }
        };

        let samples = match self.buffer_track(channel, ui, &metadata).await {
            Ok(samples) => samples,
            Err(failure) => {
                tracing::error!(
                    "Failed to download {}: {}",
                    metadata.to_string(),
                    failure.error
                );
                return Ok(TrackOutcome::Failed {
                    error: failure.error,
                    name: Some(metadata.to_string()),
                    path: Some(relative_path),
                    unavailable: failure.unavailable,
                });
            }
        };

        tracing::info!("Encoding track: {}", metadata.to_string());

        let encoder = crate::encoder::get_encoder(options.format);
        let encoded = match encoder.encode(samples).await {
            Ok(encoded) => encoded,
            Err(err) => {
                return Ok(TrackOutcome::Failed {
                    error: format!("{err:#}"),
                    name: Some(metadata.to_string()),
                    path: Some(relative_path),
                    unavailable: false,
                });
            }
        };

        tracing::info!(
            "Writing track: {:?} to file: {}",
            metadata.to_string(),
            &path
        );
        let written: Result<u64> = async {
            encoded.write_to_file(&path).await?;

            let tags = metadata.tags().await?;
            // The size on disk is read before the path is consumed by the tag writer.
            let bytes = tokio::fs::metadata(&path).await?.len();
            encoder::tags::store_tags(path, &tags, options.format).await?;
            Ok(bytes)
        }
        .await;

        match written {
            Ok(bytes) => Ok(TrackOutcome::Downloaded {
                path: relative_path,
                bytes,
            }),
            Err(err) => Ok(TrackOutcome::Failed {
                error: format!("{err:#}"),
                name: Some(metadata.to_string()),
                path: Some(relative_path),
                unavailable: false,
            }),
        }
    }

    /// The M3U comment that keeps a failed song visible in its playlist.
    fn failed_m3u_entry(error: &str, path: Option<&str>, id: &SpotifyUri) -> String {
        match path {
            Some(path) => format!("#FAILED: {error} — would have been saved as \"{path}\""),
            None => format!("#FAILED: {error} — track {id}"),
        }
    }

    async fn buffer_track(
        &self,
        mut rx: StreamEventChannel,
        ui: &DownloadUi,
        metadata: &TrackMetadata,
    ) -> Result<Samples, TrackFailure> {
        let mut samples = Vec::<i32>::new();
        while let Some(event) = rx.recv().await {
            match event {
                StreamEvent::Write {
                    bytes,
                    total,
                    mut content,
                } => {
                    tracing::trace!("Written {} bytes out of {}", bytes, total);
                    ui.track_bytes(bytes as u64);
                    samples.append(&mut content);
                }
                StreamEvent::Finished => {
                    tracing::info!("Finished downloading track");
                    break;
                }
                StreamEvent::Error(stream_error) => {
                    tracing::error!("Error while streaming track: {:?}", stream_error);
                    return Err(match stream_error {
                        StreamError::Unavailable(message) => TrackFailure {
                            error: message,
                            unavailable: true,
                        },
                        other => TrackFailure {
                            error: other.to_string(),
                            unavailable: false,
                        },
                    });
                }
                StreamEvent::Retry {
                    attempt,
                    max_attempts,
                } => {
                    tracing::warn!(
                        "Retrying download, attempt {} of {}: {}",
                        attempt,
                        max_attempts,
                        metadata.to_string()
                    );
                }
            }
        }
        Ok(Samples {
            samples,
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track_id() -> SpotifyUri {
        SpotifyUri::from_uri("spotify:track:0000000000000000000000").unwrap()
    }

    #[test]
    fn failed_m3u_entry_with_known_path_mentions_the_file() {
        let entry = Downloader::failed_m3u_entry(
            "Track is unavailable",
            Some("Artist/Album/Song.mp3"),
            &track_id(),
        );
        assert_eq!(
            entry,
            "#FAILED: Track is unavailable — would have been saved as \"Artist/Album/Song.mp3\""
        );
    }

    #[test]
    fn failed_m3u_entry_without_metadata_falls_back_to_the_id() {
        let entry = Downloader::failed_m3u_entry("Track is unavailable", None, &track_id());
        assert_eq!(
            entry,
            "#FAILED: Track is unavailable — track spotify:track:0000000000000000000000"
        );
    }
}
