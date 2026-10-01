use spotify_dl::download::{DownloadOptions, Downloader};
use spotify_dl::encoder::Format;
use spotify_dl::lock::InstanceLock;
use spotify_dl::log;
use spotify_dl::rate_limit::{PersistentRateLimiter, RateLimitConfig};
use spotify_dl::session::create_session;
use spotify_dl::track::get_tracks;
use spotify_dl::utils::get_dot_path;
use std::sync::Arc;
use structopt::StructOpt;

#[derive(Debug, StructOpt)]
#[structopt(
    name = "spotify-dl",
    about = "A commandline utility to download music directly from Spotify"
)]
struct Opt {
    #[structopt(
        help = "A list of Spotify URIs or URLs (songs, podcasts, playlists or albums)",
        required = true
    )]
    tracks: Vec<String>,
    #[structopt(
        short = "d",
        long = "destination",
        help = "The directory where the songs will be downloaded"
    )]
    destination: Option<String>,
    #[structopt(
        short = "f",
        long = "format",
        help = "The format to download the tracks in. Default is mp3.",
        default_value = "mp3"
    )]
    format: Format,
    #[structopt(
        short = "F",
        long = "force",
        help = "Force download even if the file already exists"
    )]
    force: bool,
    #[structopt(
        short = "r",
        long = "no-rate-limit",
        help = "Don't rate limit downloads (at most 30 downloads per 30 minutes)"
    )]
    no_rate_limit: bool,
}

pub fn create_destination_if_required(destination: Option<String>) -> anyhow::Result<()> {
    if let Some(destination) = destination {
        if !std::path::Path::new(&destination).exists() {
            tracing::info!("Creating destination directory: {}", destination);
            std::fs::create_dir_all(destination)?;
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    log::configure_logger()?;

    let opt = Opt::from_args();

    let dot_path = get_dot_path()?;
    let _instance_lock = match InstanceLock::acquire(dot_path.join("spotify-dl.lock")) {
        Ok(lock) => lock,
        Err(err) => {
            eprintln!("Another spotify-dl instance appears to be running: {err}");
            std::process::exit(1);
        }
    };

    let rate_limiter = if opt.no_rate_limit {
        None
    } else {
        Some(Arc::new(PersistentRateLimiter::new(
            dot_path.join("download_rate_limit.json"),
            RateLimitConfig::default(),
        )?))
    };

    create_destination_if_required(opt.destination.clone())?;

    if opt.tracks.is_empty() {
        eprintln!("No tracks provided");
        std::process::exit(1);
    }

    let session = create_session().await?;

    let track = get_tracks(opt.tracks, &session).await?;

    let downloader = Downloader::new(session);
    downloader
        .download_tracks(
            track,
            &DownloadOptions::new(
                opt.destination,
                opt.format,
                opt.force,
                !opt.no_rate_limit,
                rate_limiter,
            ),
        )
        .await
}
