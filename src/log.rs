use std::collections::VecDeque;
use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::Result;
use once_cell::sync::OnceCell;
use tracing::level_filters::LevelFilter;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::Layer;
use tracing_subscriber::Registry;
use tracing_subscriber::filter;
use tracing_subscriber::fmt;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use crate::utils::get_dot_path;

static LOG_GUARD: OnceCell<WorkerGuard> = OnceCell::new();

const MAX_LOG_SIZE: u64 = 5 * 1024 * 1024; // 5 MB

/// The log file name for a run, timestamped with the current UTC date/time
/// (`:` is avoided so the name is valid on every platform).
fn log_file_name(now_epoch_secs: u64) -> String {
    let (year, month, day) = civil_from_days((now_epoch_secs / 86_400) as i64);
    let secs_of_day = now_epoch_secs % 86_400;
    let (hour, minute, second) = (
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60,
    );
    format!("spotify-dl-{year:04}-{month:02}-{day:02}_{hour:02}-{minute:02}-{second:02}.log")
}

/// Converts days since 1970-01-01 to a proleptic Gregorian civil date
/// (Howard Hinnant's `civil_from_days` algorithm).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = (z - era * 146_097) as u64; // [0, 146096]
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era as i64 + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// In-memory ring buffer of formatted log lines, rendered by the TUIs.
static MEMORY_LOG: OnceLock<Mutex<VecDeque<String>>> = OnceLock::new();

const MEMORY_LOG_CAPACITY: usize = 1000;

/// Whether a full-screen TUI currently owns the terminal. While active, the
/// console log layer writes nowhere (the TUI renders the memory log instead).
static TUI_ACTIVE: AtomicBool = AtomicBool::new(false);

pub fn set_tui_active(active: bool) {
    TUI_ACTIVE.store(active, Ordering::Release);
}

fn memory_log() -> &'static Mutex<VecDeque<String>> {
    MEMORY_LOG.get_or_init(|| Mutex::new(VecDeque::with_capacity(MEMORY_LOG_CAPACITY)))
}

fn push_memory_line(line: &str) {
    let line = line.trim();
    if line.is_empty() {
        return;
    }
    let mut log = memory_log()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if log.len() >= MEMORY_LOG_CAPACITY {
        log.pop_front();
    }
    log.push_back(line.to_string());
}

/// The most recent log lines (at most `max`), oldest first.
pub fn recent_lines(max: usize) -> Vec<String> {
    let log = memory_log()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let start = log.len().saturating_sub(max);
    log.iter().skip(start).cloned().collect()
}

/// How many warnings/errors are replayed to the console when a TUI closes.
const REPLAY_MAX: usize = 100;

/// Restores console logging after a TUI closes and replays the warnings and
/// errors that were logged while the TUI owned the terminal, so they are
/// visible on the command line as well (the log file keeps all of them).
pub fn end_tui() {
    set_tui_active(false);

    let lines = warnings_and_errors(usize::MAX);
    if lines.is_empty() {
        return;
    }

    let skipped = lines.len().saturating_sub(REPLAY_MAX);
    if skipped > 0 {
        println!("… {skipped} earlier warning(s)/error(s) omitted, see the log file");
    }
    for line in lines.iter().skip(skipped) {
        println!("{line}");
    }
}

/// The WARN and ERROR lines currently in the memory log, oldest first, at
/// most `max` of them.
fn warnings_and_errors(max: usize) -> Vec<String> {
    let log = memory_log()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let warnings = log.iter().filter(|line| is_warning_or_error(line));
    let total = warnings.count();
    let start = total.saturating_sub(max);
    log.iter()
        .filter(|line| is_warning_or_error(line))
        .skip(start)
        .cloned()
        .collect()
}

/// The memory log's lines start with the (right-aligned) level token, since
/// the layer writes without timestamp and target.
fn is_warning_or_error(line: &str) -> bool {
    let level = line.trim_start();
    level.starts_with("ERROR") || level.starts_with("WARN")
}

#[derive(Clone)]
struct RotatingFileWriter {
    inner: Arc<Mutex<File>>,
    path: PathBuf,
}

impl RotatingFileWriter {
    fn new(path: impl Into<PathBuf>) -> io::Result<Self> {
        let path = path.into();
        std::fs::create_dir_all(path.parent().unwrap())?;
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        Ok(Self {
            inner: Arc::new(Mutex::new(file)),
            path,
        })
    }
}

impl Write for RotatingFileWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut file = self.inner.lock().unwrap();

        let metadata = file.metadata()?;
        if metadata.len() > MAX_LOG_SIZE {
            self.rotate(&mut file)?;
        }

        file.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut file = self.inner.lock().unwrap();
        file.flush()
    }
}

impl RotatingFileWriter {
    /// Moves the current chunk to `<name>.1` and starts a fresh file, so older
    /// logs stay analyzable after a rotation. Falls back to truncating when
    /// the rename is not possible (e.g. the file is locked on Windows).
    fn rotate(&self, file: &mut File) -> io::Result<()> {
        let backup = backup_path(&self.path);
        if fs::rename(&self.path, &backup).is_ok() {
            *file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)?;
        } else {
            *file = OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(&self.path)?;
        }
        Ok(())
    }
}

fn backup_path(path: &Path) -> PathBuf {
    PathBuf::from(format!("{}.1", path.display()))
}

pub fn configure_logger() -> Result<()> {
    // Every run writes its own log file, timestamped with the start time.
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|err| anyhow::anyhow!("System clock error: {err}"))?
        .as_secs();
    let path = get_dot_path()?.join(log_file_name(now));

    let writer = RotatingFileWriter::new(path)?;
    let (non_blocking, guard) = tracing_appender::non_blocking(writer);
    LOG_GUARD.set(guard).ok();

    let targets = filter::Targets::new()
        .with_target("spotify_dl", tracing::Level::DEBUG)
        .with_default(LevelFilter::OFF);

    let console_layer = fmt::layer()
        .with_target(false)
        .with_writer(ConsoleWriter)
        .with_filter(
            EnvFilter::builder()
                .with_default_directive(LevelFilter::INFO.into())
                .from_env_lossy(),
        );

    let memory_layer = fmt::layer()
        .without_time()
        .with_target(false)
        .with_ansi(false)
        .with_writer(MemoryLogWriter)
        .with_filter(
            EnvFilter::builder()
                .with_default_directive(LevelFilter::INFO.into())
                .from_env_lossy(),
        );

    let file_layer = fmt::layer()
        .with_writer(non_blocking)
        .with_ansi(false)
        // The file is the analysis artifact: everything from this crate at
        // debug level, plus info from the libraries it drives.
        .with_filter(EnvFilter::new("spotify_dl=debug,info"));

    Registry::default()
        .with(console_layer)
        .with(memory_layer)
        .with(file_layer)
        .with(targets)
        .init();

    install_panic_hook();

    Ok(())
}

/// Logs panics into the log file (and keeps the default stderr output) so
/// crashed runs can be analyzed afterwards. Hooks installed later (e.g. by
/// the TUIs) chain back to this one.
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let message = info
            .payload()
            .downcast_ref::<&str>()
            .map(|message| message.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown panic payload".to_string());
        let location = info
            .location()
            .map(|location| location.to_string())
            .unwrap_or_default();
        tracing::error!("panic at {location}: {message}");
        previous(info);
    }));
}

/// Writes console log output unless a TUI owns the terminal.
struct ConsoleWriter;

impl<'a> MakeWriter<'a> for ConsoleWriter {
    type Writer = ConsoleSink;

    fn make_writer(&'a self) -> Self::Writer {
        if TUI_ACTIVE.load(Ordering::Acquire) {
            ConsoleSink::Sink
        } else {
            ConsoleSink::Stdout
        }
    }
}

enum ConsoleSink {
    Sink,
    Stdout,
}

impl Write for ConsoleSink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            ConsoleSink::Sink => Ok(buf.len()),
            ConsoleSink::Stdout => std::io::stdout().write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            ConsoleSink::Sink => Ok(()),
            ConsoleSink::Stdout => std::io::stdout().flush(),
        }
    }
}

/// Appends formatted log lines to the in-memory ring buffer.
struct MemoryLogWriter;

impl<'a> MakeWriter<'a> for MemoryLogWriter {
    type Writer = MemoryWriter;

    fn make_writer(&'a self) -> Self::Writer {
        MemoryWriter { buf: String::new() }
    }
}

struct MemoryWriter {
    buf: String,
}

impl Write for MemoryWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buf.push_str(&String::from_utf8_lossy(buf));
        while let Some(end) = self.buf.find('\n') {
            let line: String = self.buf.drain(..=end).collect();
            push_memory_line(line.trim_end());
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn rotation_keeps_a_backup_of_the_previous_chunk() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.log");
        let mut writer = RotatingFileWriter::new(path.clone()).unwrap();

        let chunk = vec![b'x'; 4096];
        let chunks = MAX_LOG_SIZE / chunk.len() as u64 + 2;
        for _ in 0..chunks {
            writer.write_all(&chunk).unwrap();
        }
        writer.flush().unwrap();

        let backup = backup_path(&path);
        assert!(backup.exists(), "rotation must keep a backup file");
        assert!(fs::metadata(&backup).unwrap().len() >= MAX_LOG_SIZE);
        let current = fs::metadata(&path).unwrap().len();
        assert!(current < MAX_LOG_SIZE, "the current chunk must be fresh");
    }

    #[test]
    fn log_file_name_is_timestamped() {
        assert_eq!(log_file_name(0), "spotify-dl-1970-01-01_00-00-00.log");
        assert_eq!(log_file_name(86_400), "spotify-dl-1970-01-02_00-00-00.log");
        assert_eq!(
            log_file_name(86_400 + 3_600 + 60 + 1),
            "spotify-dl-1970-01-02_01-01-01.log"
        );
    }

    #[test]
    fn civil_from_days_handles_leap_years() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(59), (1970, 3, 1));
        // 1972 is a leap year: Feb 29 is day 789 after the epoch.
        assert_eq!(civil_from_days(789), (1972, 2, 29));
    }

    #[test]
    fn warnings_and_errors_are_filtered_and_capped() {
        // Reset the shared memory log for this test.
        *memory_log().lock().unwrap() = VecDeque::new();

        push_memory_line(" INFO starting");
        push_memory_line(
            "ERROR download_track{track=x}: Failed to get metadata: Error { kind: InvalidArgument }",
        );
        push_memory_line(" WARN something odd");
        push_memory_line(" INFO more context");
        push_memory_line("ERROR second failure");

        let all = warnings_and_errors(usize::MAX);
        assert_eq!(all.len(), 3);
        assert!(all[0].starts_with("ERROR"));
        assert!(all[1].starts_with("WARN"));
        assert!(all[2].starts_with("ERROR"));

        // The cap keeps the most recent entries.
        let capped = warnings_and_errors(2);
        assert_eq!(capped.len(), 2);
        assert!(capped[0].starts_with("WARN"));
        assert!(capped[1].starts_with("ERROR"));

        *memory_log().lock().unwrap() = VecDeque::new();
    }
}
