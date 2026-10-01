use std::collections::VecDeque;
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

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
    if line.trim().is_empty() {
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
            // Truncate the file
            *file = OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(&self.path)?;
        }

        file.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut file = self.inner.lock().unwrap();
        file.flush()
    }
}

pub fn configure_logger() -> Result<()> {
    let path = get_dot_path()?.join("spotify-dl.log");

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
        .with_filter(EnvFilter::new("info"));

    Registry::default()
        .with(console_layer)
        .with(memory_layer)
        .with(file_layer)
        .with(targets)
        .init();

    Ok(())
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
