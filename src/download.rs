use std::fmt::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use indicatif::MultiProgress;
use indicatif::ProgressBar;
use indicatif::ProgressState;
use indicatif::ProgressStyle;
use librespot::core::session::Session;

use crate::encoder;
use crate::encoder::Format;
use crate::encoder::Samples;
use crate::rate_limit::PersistentRateLimiter;
use crate::stream::Stream;
use crate::stream::StreamEvent;
use crate::stream::StreamEventChannel;
use crate::track::Track;
use crate::track::TrackMetadata;

pub struct Downloader {
    session: Session,
    progress_bar: MultiProgress,
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
        Downloader {
            session,
            progress_bar: MultiProgress::new(),
        }
    }

    pub async fn download_tracks(
        self,
        tracks: Vec<Track>,
        options: &DownloadOptions,
    ) -> Result<()> {
        if options.rate_limit {
            tracing::info!("Rate limiting enabled: at most 30 downloads per 30 minutes");
        }

        let mut filenames = Vec::new();
        for track in tracks.into_iter() {
            let filename = match self.download_track(&track, options).await {
                Err(err) => {
                    tracing::warn!("Error in track {:?}: {:?}", track.id, err);
                    "".to_string()
                }
                Ok(filename) => filename,
            };
            filenames.push((track.source_playlist.clone(), filename));
        }

        let mut playlists: Vec<(String, Vec<String>)> = Vec::new();
        for (source_playlist, filename) in filenames {
            let Some(playlist) = source_playlist else {
                continue;
            };
            if filename.is_empty() {
                continue;
            }
            match playlists.iter_mut().find(|(name, _)| name == &playlist) {
                Some((_, entries)) => entries.push(filename),
                None => playlists.push((playlist, vec![filename])),
            }
        }

        for (playlist, entries) in playlists {
            let mut content = String::from("#EXTM3U\n");
            for entry in entries {
                content.push_str(&entry);
                content.push('\n');
            }
            let playlist_file = options.destination.join(format!("{}.m3u", playlist));
            tokio::fs::write(&playlist_file, content).await?;
            tracing::info!("Wrote playlist file: {:?}", playlist_file);
        }
        Ok(())
    }

    #[tracing::instrument(
        name = "download_track",
        skip(self, track, options),
        fields(track = %track.id)
    )]
    async fn download_track(&self, track: &Track, options: &DownloadOptions) -> Result<String> {
        let metadata = track.metadata(&self.session).await?;
        tracing::info!("Downloading track: {:?}", metadata.track_name);

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
            return Ok(relative_path);
        }

        // Only actual downloads are rate limited, not metadata requests or
        // tracks skipped because they already exist on disk.
        if options.rate_limit {
            if let Some(rate_limiter) = &options.rate_limiter {
                rate_limiter.acquire().await?;
            }
        }

        let pb = self.add_progress_bar(&metadata);

        let stream = Stream::new(self.session.clone());
        let channel = match stream.stream(track).await {
            Ok(channel) => channel,
            Err(e) => {
                self.fail_with_error(&pb, &metadata.to_string(), e.to_string());
                return Ok("".to_string());
            }
        };

        let samples = match self.buffer_track(channel, &pb, &metadata).await {
            Ok(samples) => samples,
            Err(e) => {
                self.fail_with_error(&pb, &metadata.to_string(), e.to_string());
                return Ok("".to_string());
            }
        };

        tracing::info!("Encoding track: {}", metadata.to_string());
        pb.set_message(format!("Encoding {}", metadata.to_string()));

        let encoder = crate::encoder::get_encoder(options.format);
        let stream = encoder.encode(samples).await?;

        pb.set_message(format!("Writing {}", metadata.to_string()));
        tracing::info!(
            "Writing track: {:?} to file: {}",
            metadata.to_string(),
            &path
        );
        stream.write_to_file(&path).await?;

        let tags = metadata.tags().await?;
        encoder::tags::store_tags(path, &tags, options.format).await?;

        pb.finish_with_message(format!("Downloaded {}", metadata.to_string()));
        Ok(relative_path)
    }

    fn add_progress_bar(&self, track: &TrackMetadata) -> ProgressBar {
        let pb = self
            .progress_bar
            .add(ProgressBar::new(track.approx_size() as u64));
        pb.enable_steady_tick(Duration::from_millis(100));
        pb.set_style(ProgressStyle::with_template("{spinner:.green} {msg} [{elapsed_precise}] [{wide_bar:.cyan/blue}] {bytes}/{total_bytes} ({eta})")
            // Infallible
            .unwrap()
            .with_key("eta", |state: &ProgressState, w: &mut dyn Write| write!(w, "{:.1}s", state.eta().as_secs_f64()).unwrap())
            .progress_chars("#>-"));
        pb.set_message(track.to_string());
        pb
    }

    async fn buffer_track(
        &self,
        mut rx: StreamEventChannel,
        pb: &ProgressBar,
        metadata: &TrackMetadata,
    ) -> Result<Samples> {
        let mut samples = Vec::<i32>::new();
        while let Some(event) = rx.recv().await {
            match event {
                StreamEvent::Write {
                    bytes,
                    total,
                    mut content,
                } => {
                    tracing::trace!("Written {} bytes out of {}", bytes, total);
                    pb.set_position(bytes as u64);
                    samples.append(&mut content);
                }
                StreamEvent::Finished => {
                    tracing::info!("Finished downloading track");
                    break;
                }
                StreamEvent::Error(stream_error) => {
                    tracing::error!("Error while streaming track: {:?}", stream_error);
                    return Err(anyhow::anyhow!("Streaming error: {:?}", stream_error));
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
                    pb.set_message(format!(
                        "Retrying ({}/{}) {}",
                        attempt,
                        max_attempts,
                        metadata.to_string()
                    ));
                }
            }
        }
        Ok(Samples {
            samples,
            ..Default::default()
        })
    }

    fn fail_with_error<S>(&self, pb: &ProgressBar, name: &str, e: S)
    where
        S: Into<String>,
    {
        tracing::error!("Failed to download {}: {}", name, e.into());
        pb.finish_with_message(
            console::style(format!("Failed! {}", name))
                .red()
                .to_string(),
        );
    }
}
