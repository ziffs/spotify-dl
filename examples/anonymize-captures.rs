//! Anonymizes a capture directory (raw API responses recorded with
//! `SPOTIFY_DL_CAPTURE_DIR`) into mock fixtures for tests and offline runs.
//!
//! Usage: `cargo run --example anonymize-captures -- <capture-dir> <fixtures-dir> [seed]`

use std::path::PathBuf;

use spotify_dl::capture::DEFAULT_ANONYMIZE_SEED;
use spotify_dl::capture::anonymize_capture_dir;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!(
            "usage: anonymize-captures <capture-dir> <fixtures-dir> [seed]\n\
             (default seed: {DEFAULT_ANONYMIZE_SEED} — a fixed seed keeps fixtures stable)"
        );
        std::process::exit(1);
    }

    let src = PathBuf::from(&args[1]);
    let dst = PathBuf::from(&args[2]);
    let seed = match args.get(3) {
        Some(seed) => seed.parse::<u64>().expect("seed must be a u64"),
        None => DEFAULT_ANONYMIZE_SEED,
    };

    let summary = anonymize_capture_dir(&src, &dst, seed)?;
    println!(
        "Anonymized {} rootlist page(s) and {} playlist(s) ({} folder(s)) into {}",
        summary.rootlist_pages,
        summary.playlists,
        summary.folders,
        dst.display()
    );
    Ok(())
}
