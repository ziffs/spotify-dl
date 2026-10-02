use spotify_dl::account::AccountSource;
use spotify_dl::account::mark_playlists_downloaded;
use spotify_dl::account::select_folder_playlists;
use spotify_dl::download::DownloadOptions;
use spotify_dl::download::Downloader;
use spotify_dl::download_ui;
use spotify_dl::encoder::Format;
use spotify_dl::lock::InstanceLock;
use spotify_dl::log;
use spotify_dl::rate_limit::{PersistentRateLimiter, RateLimitConfig};
use spotify_dl::session::create_session;
use spotify_dl::track::get_tracks;
use spotify_dl::utils::get_dot_path;
use std::path::PathBuf;
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
        required_unless = "from-account"
    )]
    tracks: Vec<String>,
    #[structopt(
        long = "from-account",
        conflicts_with = "tracks",
        help = "Browse the playlist folders of the logged-in account and pick one to download"
    )]
    from_account: bool,
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

    tracing::info!(
        "spotify-dl {} starting — destination: {:?}, format: {:?}, force: {}, rate limit: {}, from-account: {}, mock: {:?}",
        env!("CARGO_PKG_VERSION"),
        opt.destination,
        opt.format,
        opt.force,
        !opt.no_rate_limit,
        opt.from_account,
        std::env::var_os("SPOTIFY_DL_MOCK_DIR"),
    );

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

    if !opt.from_account && opt.tracks.is_empty() {
        eprintln!("No tracks provided");
        std::process::exit(1);
    }

    let mock_dir = std::env::var_os("SPOTIFY_DL_MOCK_DIR").map(PathBuf::from);
    if mock_dir.is_some() && !opt.from_account {
        eprintln!("SPOTIFY_DL_MOCK_DIR only supports --from-account");
        std::process::exit(1);
    }
    let capture_dir = std::env::var_os("SPOTIFY_DL_CAPTURE_DIR").map(PathBuf::from);

    let (source, session) = if let Some(dir) = mock_dir {
        (AccountSource::mock(dir), None)
    } else {
        let session = create_session().await?;
        (
            AccountSource::live(session.clone(), capture_dir),
            Some(session),
        )
    };

    let selection = if opt.from_account {
        Some(select_folder_playlists(&source).await?)
    } else {
        None
    };

    let tracks = selection.clone().unwrap_or_else(|| opt.tracks.clone());

    if tracks.is_empty() {
        eprintln!("No tracks provided");
        std::process::exit(1);
    }

    // Mock mode has no session: it stops after the picker and only reports
    // what would have been downloaded.
    let Some(session) = session else {
        println!(
            "Mock mode: would download {} playlist{}:",
            tracks.len(),
            if tracks.len() == 1 { "" } else { "s" }
        );
        for track in &tracks {
            println!("  - {track}");
        }
        return Ok(());
    };

    let track = get_tracks(tracks, &session).await?;

    // The download runs as a task while a dedicated thread renders the
    // download view; the view exits when the task finishes.
    let (local_answer_tx, local_answer_rx) = tokio::sync::mpsc::channel(4);
    let ui = download_ui::DownloadUi::new(&track, local_answer_tx, local_answer_rx);
    let (finished_tx, finished_rx) = std::sync::mpsc::channel::<()>();
    let download_options = DownloadOptions::new(
        opt.destination,
        opt.format,
        opt.force,
        !opt.no_rate_limit,
        rate_limiter.clone(),
    );

    let download_ui_handle = ui.clone();
    let download_task = tokio::spawn(async move {
        let downloader = Downloader::new(session);
        let result = downloader
            .download_tracks(track, &download_options, &download_ui_handle)
            .await;
        let _ = finished_tx.send(());
        result
    });

    let tui_ui = ui.clone();
    let tui_task = tokio::task::spawn_blocking(move || {
        download_ui::run_tui(tui_ui, rate_limiter, finished_rx)
    });

    let (download_result, tui_result) = tokio::join!(download_task, tui_task);
    tui_result.map_err(|err| anyhow::anyhow!("download view crashed: {err}"))??;

    // The summary is printed after the view closes, so the terminal shows
    // more than just the logs from before it opened.
    let summary = ui.summary();
    println!("\n{summary}");
    tracing::info!("{summary}");

    download_result.map_err(|err| anyhow::anyhow!("download task crashed: {err}"))??;

    // Record the downloaded status once the run has finished, so the picker
    // can show which playlists are already on disk.
    if let Some(selected) = selection
        && let Err(err) = mark_playlists_downloaded(&selected)
    {
        tracing::warn!("Could not save the downloaded status: {err:#}");
    }

    Ok(())
}
