//! Shared progress state and the full-screen TUI for the download process.
//!
//! The download loop updates a [`DownloadUi`] (playlists, overall progress,
//! current track, abort flag) while a dedicated thread renders it with
//! ratatui: a log panel fed from the in-memory log ring buffer, the playlist
//! list with completion percentages, an overall progress bar and a rate-limit
//! bar. Because a song can appear in several playlists, completing a song
//! updates the percentage of every playlist that contains it.

use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::mpsc::TryRecvError;
use std::time::Duration;
use tokio::sync::mpsc as tokio_mpsc;

use anyhow::Result;
use anyhow::anyhow;
use librespot::core::SpotifyUri;
use ratatui::Frame;
use ratatui::crossterm::event as crossterm_event;
use ratatui::crossterm::event::Event;
use ratatui::crossterm::event::KeyCode;
use ratatui::crossterm::event::KeyEventKind;
use ratatui::crossterm::event::KeyModifiers;
use ratatui::layout::Constraint;
use ratatui::layout::Layout;
use ratatui::style::Color;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Block;
use ratatui::widgets::Borders;
use ratatui::widgets::Gauge;
use ratatui::widgets::List;
use ratatui::widgets::ListItem;
use ratatui::widgets::ListState;
use ratatui::widgets::Paragraph;

use crate::local_match::LocalTrackInfo;
use crate::log;
use crate::rate_limit::PersistentRateLimiter;
use crate::track::Track;

/// How often the download TUI redraws and polls for keys.
const TICK: Duration = Duration::from_millis(200);

/// A question the download loop asks the user through the TUI.
#[derive(Debug, Clone)]
pub enum PendingQuestion {
    /// Pick the folder that contains the local files.
    SelectFolder,
    /// Confirm which local file corresponds to a `spotify:local:` entry.
    MatchTrack {
        info: LocalTrackInfo,
        playlist: Option<String>,
        candidates: Vec<PathBuf>,
    },
}

/// The user's answer to a pending question.
#[derive(Debug)]
pub enum PendingAnswer {
    /// The folder chosen for local file matching.
    Folder(PathBuf),
    /// The user declined to pick a folder; local files are skipped this run.
    Declined,
    /// The confirmed candidate file, or none when the user skips.
    Match(Option<PathBuf>),
}

#[derive(Debug)]
struct PlaylistProgress {
    name: String,
    /// Unique songs in the playlist.
    total: usize,
    done: usize,
}

#[derive(Debug)]
struct CurrentTrack {
    name: String,
    bytes: u64,
    total_bytes: u64,
}

#[derive(Debug, Default)]
struct State {
    playlists: Vec<PlaylistProgress>,
    /// Song id -> indices into `playlists` (a song can be in several).
    track_playlists: HashMap<SpotifyUri, Vec<usize>>,
    completed: HashSet<SpotifyUri>,
    /// Unique songs across all entries (with and without a playlist).
    total_unique: usize,
    current: Option<CurrentTrack>,
    /// Playlist the list view follows while a song is downloading.
    current_playlist: Option<usize>,
    aborted: bool,
    finished: bool,
    /// Songs written to disk by this run.
    downloaded_new: usize,
    /// Songs that were already on disk and got skipped.
    skipped: usize,
    /// Size on disk of the songs written by this run.
    new_bytes: u64,
    /// Playlists whose file was written by this run.
    playlists_synced: HashSet<String>,
    /// The question currently shown to the user, if any.
    pending_question: Option<PendingQuestion>,
    /// How many questions were asked and answered so far.
    questions_total: usize,
    questions_answered: usize,
}

/// What a finished (or aborted) download run produced.
#[derive(Debug, Clone)]
pub struct DownloadSummary {
    /// Playlists whose file was written by this run.
    pub playlists_synced: usize,
    /// Unique songs in the run.
    pub titles_total: usize,
    /// Songs written to disk by this run.
    pub downloaded: usize,
    /// Songs that were already on disk and got skipped.
    pub skipped: usize,
    /// Songs that could not be downloaded.
    pub failed: usize,
    /// Size on disk of the songs written by this run.
    pub new_bytes: u64,
}

impl fmt::Display for DownloadSummary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Synced {} playlist{}, {} title(s) total, {} downloaded",
            self.playlists_synced,
            if self.playlists_synced == 1 { "" } else { "s" },
            self.titles_total,
            self.downloaded,
        )?;
        if self.skipped > 0 {
            write!(f, ", {} already on disk", self.skipped)?;
        }
        if self.failed > 0 {
            write!(f, ", {} failed", self.failed)?;
        }
        write!(f, ", {} new on disk", format_bytes(self.new_bytes))
    }
}

fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let bytes = bytes as f64;
    if bytes >= GB {
        format!("{:.1} GB", bytes / GB)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes / MB)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes / KB)
    } else {
        format!("{bytes} B")
    }
}

/// Shared handle between the download loop and the download TUI.
#[derive(Clone)]
pub struct DownloadUi {
    state: Arc<Mutex<State>>,
    abort: Arc<AtomicBool>,
    /// Answers from the TUI, consumed by the download loop.
    local_answers: Arc<tokio::sync::Mutex<tokio_mpsc::Receiver<PendingAnswer>>>,
    local_answer_tx: tokio_mpsc::Sender<PendingAnswer>,
}

impl DownloadUi {
    /// Builds the progress model from the track entries that are about to be
    /// downloaded. A song appearing in several playlists counts once per
    /// playlist, and once overall.
    pub fn new(
        tracks: &[Track],
        local_answer_tx: tokio_mpsc::Sender<PendingAnswer>,
        local_answers: tokio_mpsc::Receiver<PendingAnswer>,
    ) -> Self {
        let mut state = State::default();
        let mut playlist_index: HashMap<String, usize> = HashMap::new();
        let mut counted: HashSet<(usize, SpotifyUri)> = HashSet::new();
        let mut unique: HashSet<SpotifyUri> = HashSet::new();

        for track in tracks {
            unique.insert(track.id.clone());
            let Some(playlist) = &track.source_playlist else {
                continue;
            };
            let index = *playlist_index.entry(playlist.clone()).or_insert_with(|| {
                state.playlists.push(PlaylistProgress {
                    name: playlist.clone(),
                    total: 0,
                    done: 0,
                });
                state.playlists.len() - 1
            });
            if counted.insert((index, track.id.clone())) {
                state.playlists[index].total += 1;
                state
                    .track_playlists
                    .entry(track.id.clone())
                    .or_default()
                    .push(index);
            }
        }
        state.total_unique = unique.len();

        Self {
            state: Arc::new(Mutex::new(state)),
            abort: Arc::new(AtomicBool::new(false)),
            local_answers: Arc::new(tokio::sync::Mutex::new(local_answers)),
            local_answer_tx,
        }
    }

    fn with_state<R>(&self, f: impl FnOnce(&mut State) -> R) -> R {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        f(&mut state)
    }

    pub fn is_aborted(&self) -> bool {
        self.abort.load(Ordering::Acquire)
    }

    /// Requests a graceful abort; the download loop stops after the current song.
    pub fn abort(&self) {
        self.abort.store(true, Ordering::Release);
        self.with_state(|state| state.aborted = true);
    }

    /// The song started downloading; the URI is a fallback display until the
    /// metadata arrives.
    pub fn track_started(&self, track: &Track) {
        self.with_state(|state| {
            state.current = Some(CurrentTrack {
                name: format!("{}", track.id),
                bytes: 0,
                total_bytes: 0,
            });
            state.current_playlist = state
                .track_playlists
                .get(&track.id)
                .and_then(|indices| indices.first().copied());
        });
    }

    /// The song's metadata arrived; refines the display name and total size.
    pub fn track_metadata(&self, display: String, total_bytes: u64) {
        self.with_state(|state| {
            if let Some(current) = &mut state.current {
                current.name = display;
                current.total_bytes = total_bytes;
            }
        });
    }

    /// Streaming progress of the current song, in bytes.
    pub fn track_bytes(&self, bytes: u64) {
        self.with_state(|state| {
            if let Some(current) = &mut state.current {
                current.bytes = bytes;
            }
        });
    }

    /// The song finished. `downloaded` tells whether it was written by this
    /// run (`bytes` is its size on disk) or was already on disk and got
    /// skipped. Every playlist that contains it gets its percentage updated.
    pub fn track_completed(&self, id: &SpotifyUri, downloaded: bool, bytes: u64) {
        self.with_state(|state| {
            if state.completed.insert(id.clone()) {
                if let Some(indices) = state.track_playlists.get(id) {
                    for &index in indices {
                        state.playlists[index].done += 1;
                    }
                }
                if downloaded {
                    state.downloaded_new += 1;
                    state.new_bytes += bytes;
                } else {
                    state.skipped += 1;
                }
            }
            state.current = None;
            state.current_playlist = None;
        });
    }

    /// Records that the playlist's file was written by this run.
    pub fn playlist_synced(&self, name: &str) {
        self.with_state(|state| {
            state.playlists_synced.insert(name.to_string());
        });
    }

    /// The outcome of the run so far.
    pub fn summary(&self) -> DownloadSummary {
        self.with_state(|state| DownloadSummary {
            playlists_synced: state.playlists_synced.len(),
            titles_total: state.total_unique,
            downloaded: state.downloaded_new,
            skipped: state.skipped,
            failed: state.total_unique - state.completed.len(),
            new_bytes: state.new_bytes,
        })
    }

    /// The download run finished (successfully or not).
    pub fn finish(&self) {
        self.with_state(|state| {
            state.finished = true;
            state.current = None;
        });
    }

    /// The playlists that contain the given song, by name.
    pub fn playlists_of(&self, id: &SpotifyUri) -> Vec<String> {
        self.with_state(|state| {
            state
                .track_playlists
                .get(id)
                .map(|indices| {
                    indices
                        .iter()
                        .map(|&index| state.playlists[index].name.clone())
                        .collect()
                })
                .unwrap_or_default()
        })
    }

    /// Shows a question in the TUI; the download loop then waits for the
    /// answer with [`recv_local_answer`](Self::recv_local_answer).
    pub fn push_question(&self, question: PendingQuestion) {
        self.with_state(|state| {
            state.pending_question = Some(question);
            state.questions_total += 1;
        });
    }

    /// The question currently shown to the user, if any.
    pub fn current_question(&self) -> Option<PendingQuestion> {
        self.with_state(|state| state.pending_question.clone())
    }

    /// How many questions were answered and asked so far.
    pub fn question_progress(&self) -> (usize, usize) {
        self.with_state(|state| (state.questions_answered, state.questions_total))
    }

    /// Waits for the user's answer to the pending question.
    pub async fn recv_local_answer(&self) -> Option<PendingAnswer> {
        self.local_answers.lock().await.recv().await
    }

    /// Sends the user's answer from the TUI thread and clears the question.
    pub fn send_local_answer(&self, answer: PendingAnswer) {
        match self.local_answer_tx.try_send(answer) {
            Ok(()) => {}
            // The download loop is gone; nothing is waiting for the answer.
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {}
            Err(err) => {
                tracing::warn!("Could not send the answer: {err}");
                return;
            }
        }
        self.with_state(|state| {
            state.pending_question = None;
            state.questions_answered += 1;
        });
    }
}

/// Renders the download TUI until the download task finishes.
///
/// A missing terminal degrades to a no-op so downloads keep working when the
/// output is piped.
pub fn run_tui(
    ui: DownloadUi,
    rate_limiter: Option<Arc<PersistentRateLimiter>>,
    mut finished: mpsc::Receiver<()>,
) -> Result<()> {
    log::set_tui_active(true);
    let result = tui_loop(ui, rate_limiter, &mut finished);
    log::end_tui();
    result
}

fn tui_loop(
    ui: DownloadUi,
    rate_limiter: Option<Arc<PersistentRateLimiter>>,
    finished: &mut mpsc::Receiver<()>,
) -> Result<()> {
    let mut terminal = match ratatui::try_init() {
        Ok(terminal) => terminal,
        Err(err) => {
            tracing::info!(
                "No terminal available for the download view ({err}); continuing without it"
            );
            return Ok(());
        }
    };

    let mut list_state = ListState::default();
    let mut question_cursor = 0usize;
    let mut browser: Option<BrowserState> = None;
    let result = loop {
        let question = ui.current_question();
        if matches!(question, Some(PendingQuestion::SelectFolder)) && browser.is_none() {
            browser = Some(BrowserState::at_home());
        }
        if question.is_none() {
            browser = None;
        }

        if let Err(err) = terminal.draw(|frame| {
            draw(
                frame,
                &ui,
                rate_limiter.as_deref(),
                &mut list_state,
                question.as_ref(),
                question_cursor,
                browser.as_ref(),
            )
        }) {
            break Err(anyhow!("download view failed: {err}"));
        }

        // Ctrl-C requests a graceful abort (waking the download loop, which
        // may be waiting for an answer); while a question is pending the keys
        // drive the question instead. A failing event poll (no controlling
        // terminal) is not fatal.
        match crossterm_event::poll(TICK) {
            Ok(true) => {
                if let Event::Key(key) = crossterm_event::read()?
                    && key.kind == KeyEventKind::Press
                {
                    if key.modifiers.contains(KeyModifiers::CONTROL)
                        && key.code == KeyCode::Char('c')
                    {
                        if let Some(question) = &question {
                            ui.send_local_answer(skip_answer(question));
                        }
                        ui.abort();
                    } else if let Some(question) = &question {
                        match key.code {
                            KeyCode::Up => question_cursor = question_cursor.saturating_sub(1),
                            KeyCode::Down => {
                                let len = question_list_len(question, browser.as_ref());
                                question_cursor = (question_cursor + 1).min(len.saturating_sub(1));
                            }
                            KeyCode::Enter => match question {
                                PendingQuestion::SelectFolder => {
                                    // Enter opens the highlighted directory;
                                    // 's' selects the current one.
                                    if let Some(next) = browser
                                        .as_ref()
                                        .and_then(|browser| browser.descend(question_cursor))
                                    {
                                        browser = Some(next);
                                    }
                                }
                                PendingQuestion::MatchTrack { candidates, .. } => {
                                    ui.send_local_answer(PendingAnswer::Match(
                                        candidates.get(question_cursor).cloned(),
                                    ));
                                    question_cursor = 0;
                                }
                            },
                            KeyCode::Esc | KeyCode::Char('s') => {
                                ui.send_local_answer(skip_answer(question));
                                question_cursor = 0;
                            }
                            _ => {}
                        }
                    }
                }
            }
            Ok(false) => {}
            Err(_) => std::thread::sleep(TICK),
        }

        match finished.try_recv() {
            Ok(()) | Err(TryRecvError::Disconnected) => break Ok(()),
            Err(TryRecvError::Empty) => {}
        }
    };

    ratatui::restore();
    result
}

/// The answer that skips the pending question.
fn skip_answer(question: &PendingQuestion) -> PendingAnswer {
    match question {
        PendingQuestion::SelectFolder => PendingAnswer::Declined,
        PendingQuestion::MatchTrack { .. } => PendingAnswer::Match(None),
    }
}

/// The number of selectable entries of the pending question.
fn question_list_len(question: &PendingQuestion, browser: Option<&BrowserState>) -> usize {
    match question {
        PendingQuestion::SelectFolder => browser.map(|browser| browser.dirs.len() + 1).unwrap_or(1),
        PendingQuestion::MatchTrack { candidates, .. } => candidates.len(),
    }
}

/// The folder browser shown while the user picks the local-files folder.
struct BrowserState {
    cwd: PathBuf,
    dirs: Vec<PathBuf>,
}

impl BrowserState {
    fn at_home() -> Self {
        let cwd = std::env::var_os("HOME")
            .map(PathBuf::from)
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from("."));
        Self::load(cwd)
    }

    fn load(cwd: PathBuf) -> Self {
        let mut dirs: Vec<PathBuf> = fs::read_dir(&cwd)
            .map(|entries| {
                entries
                    .filter_map(|entry| entry.ok())
                    .map(|entry| entry.path())
                    .filter(|path| path.is_dir())
                    .collect()
            })
            .unwrap_or_default();
        dirs.sort();
        Self { cwd, dirs }
    }

    /// Enters the entry at `cursor`: 0 is the parent directory, the rest are
    /// the subdirectories of the current one.
    fn descend(&self, cursor: usize) -> Option<Self> {
        let target = if cursor == 0 {
            self.cwd.parent().map(|parent| parent.to_path_buf())?
        } else {
            self.dirs.get(cursor - 1)?.clone()
        };
        Some(Self::load(target))
    }
}

fn draw(
    frame: &mut Frame,
    ui: &DownloadUi,
    rate_limiter: Option<&PersistentRateLimiter>,
    list_state: &mut ListState,
    question: Option<&PendingQuestion>,
    question_cursor: usize,
    browser: Option<&BrowserState>,
) {
    let area = frame.area();
    let rate_status = rate_limiter.map(|limiter| limiter.status());

    ui.with_state(|state| {
        let playlist_rows = state.playlists.len() as u16;
        let list_height = if playlist_rows == 0 {
            0
        } else {
            (playlist_rows + 2)
                .min(area.height.saturating_sub(5) / 2)
                .max(3)
        };

        let mut constraints = vec![Constraint::Min(4)];
        if list_height > 0 {
            constraints.push(Constraint::Length(list_height));
        }
        constraints.push(Constraint::Length(1)); // current track
        constraints.push(Constraint::Length(1)); // overall gauge
        if rate_status.is_some() {
            constraints.push(Constraint::Length(1)); // rate limit gauge
        }
        let chunks = Layout::vertical(constraints).split(area);

        // While a question is pending, the log shares its row with the
        // questioning pane on the right.
        let (log_area, question_area) = match question {
            Some(question) => {
                let panes =
                    Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)])
                        .split(chunks[0]);
                (panes[0], Some((panes[1], question)))
            }
            None => (chunks[0], None),
        };

        let log_height = log_area.height.saturating_sub(2) as usize;
        let lines: Vec<Line> = log::recent_lines(log_height)
            .into_iter()
            .map(Line::raw)
            .collect();
        frame.render_widget(
            Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title("Log")),
            log_area,
        );

        if let Some((area, question)) = question_area {
            draw_question(
                frame,
                area,
                question,
                question_cursor,
                browser,
                state.questions_answered,
                state.questions_total,
            );
        }

        let mut next = 1;
        if list_height > 0 {
            let list_area = chunks[next];
            next += 1;
            let width = list_area.width.saturating_sub(2) as usize;
            let items: Vec<ListItem> = state
                .playlists
                .iter()
                .map(|playlist| playlist_line(playlist, width))
                .collect();
            if let Some(index) = state.current_playlist {
                list_state.select(Some(index));
            }
            let list = List::new(items).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!("Playlists ({playlist_rows})")),
            );
            frame.render_stateful_widget(list, list_area, list_state);
        }

        let current = match &state.current {
            Some(current) if current.total_bytes > 0 => format!(
                "Downloading: {} ({}/{} MB)",
                current.name,
                current.bytes / (1024 * 1024),
                current.total_bytes / (1024 * 1024),
            ),
            Some(current) => format!("Downloading: {}", current.name),
            None if state.finished => "Done".to_string(),
            None if state.aborted => "Aborting after the current song…".to_string(),
            None => String::new(),
        };
        frame.render_widget(Paragraph::new(Line::from(current)), chunks[next]);
        next += 1;

        let done = state.completed.len();
        let total = state.total_unique;
        let overall_label = if state.aborted && !state.finished {
            format!(
                "Overall {done}/{total} ({}%) — aborting",
                percent(done, total)
            )
        } else {
            format!("Overall {done}/{total} ({}%)", percent(done, total))
        };
        let overall = Gauge::default()
            .ratio(ratio(done, total))
            .label(overall_label)
            .gauge_style(Style::default().fg(Color::Cyan));
        frame.render_widget(overall, chunks[next]);
        next += 1;

        if let Some(status) = rate_status {
            let mut label = format!("Rate limit {}/{}", status.remaining, status.capacity);
            if status.next_token_in > Duration::ZERO {
                label.push_str(&format!(
                    " · +1 in {}",
                    format_duration(status.next_token_in)
                ));
            }
            let gauge = Gauge::default()
                .ratio(f64::from(status.remaining) / f64::from(status.capacity))
                .label(label)
                .gauge_style(Style::default().fg(Color::Yellow));
            frame.render_widget(gauge, chunks[next]);
        }
    });
}

/// The questioning pane shown on the right while a question is pending.
fn draw_question(
    frame: &mut Frame,
    area: ratatui::layout::Rect,
    question: &PendingQuestion,
    cursor: usize,
    browser: Option<&BrowserState>,
    answered: usize,
    total: usize,
) {
    let title = match question {
        PendingQuestion::SelectFolder => "Select the folder with your local files".to_string(),
        PendingQuestion::MatchTrack { .. } => format!("Local files ({answered}/{total})"),
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    match question {
        PendingQuestion::SelectFolder => {
            let browser = browser.expect("browser state for the folder question");
            let rows = Layout::vertical([
                Constraint::Length(1), // current path
                Constraint::Min(1),    // directories
                Constraint::Length(1), // hints
                Constraint::Length(1), // gauge
            ])
            .split(inner);

            frame.render_widget(
                Paragraph::new(format!("Searching in: {}", browser.cwd.display())),
                rows[0],
            );

            let mut items = vec![ListItem::new("..")];
            items.extend(browser.dirs.iter().map(|dir| {
                ListItem::new(
                    dir.file_name()
                        .map(|name| name.to_string_lossy().to_string())
                        .unwrap_or_else(|| dir.display().to_string()),
                )
            }));
            let mut state = ListState::default().with_selected(Some(cursor.min(items.len() - 1)));
            frame.render_stateful_widget(
                List::new(items).block(Block::default()),
                rows[1],
                &mut state,
            );

            frame.render_widget(
                Paragraph::new(
                    "↑/↓ move · enter open · s select this folder · esc skip local files",
                ),
                rows[2],
            );
            frame.render_widget(question_gauge(answered, total), rows[3]);
        }
        PendingQuestion::MatchTrack {
            info,
            playlist,
            candidates,
        } => {
            let rows = Layout::vertical([
                Constraint::Length(1), // song
                Constraint::Length(1), // playlist
                Constraint::Min(1),    // candidates
                Constraint::Length(1), // hints
                Constraint::Length(1), // gauge
            ])
            .split(inner);

            frame.render_widget(Paragraph::new(info.display()), rows[0]);
            if let Some(playlist) = playlist {
                frame.render_widget(
                    Paragraph::new(format!("from playlist: {playlist}")),
                    rows[1],
                );
            }

            if candidates.is_empty() {
                frame.render_widget(
                    Paragraph::new("No matching files found — esc to skip"),
                    rows[2],
                );
            } else {
                let items: Vec<ListItem> = candidates
                    .iter()
                    .map(|candidate| {
                        ListItem::new(truncate(
                            &candidate.file_name().unwrap_or_default().to_string_lossy(),
                            inner.width.saturating_sub(2) as usize,
                        ))
                    })
                    .collect();
                let mut state =
                    ListState::default().with_selected(Some(cursor.min(items.len() - 1)));
                frame.render_stateful_widget(
                    List::new(items).block(Block::default()),
                    rows[2],
                    &mut state,
                );
            }

            frame.render_widget(
                Paragraph::new("↑/↓ move · enter confirm · esc skip"),
                rows[3],
            );
            frame.render_widget(question_gauge(answered, total), rows[4]);
        }
    }
}

fn question_gauge(answered: usize, total: usize) -> Gauge<'static> {
    Gauge::default()
        .ratio(ratio(answered, total))
        .label(format!("{answered}/{total} answered"))
        .gauge_style(Style::default().fg(Color::Green))
}

fn playlist_line(playlist: &PlaylistProgress, width: usize) -> ListItem<'static> {
    let pct = percent(playlist.done, playlist.total);
    let right = format!("{}/{} ({}%)", playlist.done, playlist.total, pct);
    let style = if playlist.total > 0 && playlist.done >= playlist.total {
        Style::default().fg(Color::Green)
    } else {
        Style::default().fg(Color::Cyan)
    };

    let name_width = width.saturating_sub(right.len() + 1);
    let name = truncate(&playlist.name, name_width);
    let padding = " ".repeat(name_width.saturating_sub(name.chars().count()));

    ListItem::new(Line::from(vec![
        Span::raw(name),
        Span::raw(padding),
        Span::styled(right, style),
    ]))
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let taken: String = text.chars().take(max.saturating_sub(1)).collect();
        format!("{taken}…")
    }
}

fn percent(done: usize, total: usize) -> usize {
    if total == 0 {
        return 0;
    }
    done.checked_mul(100)
        .and_then(|product| product.checked_div(total))
        .unwrap_or(0)
}

fn ratio(done: usize, total: usize) -> f64 {
    if total == 0 {
        0.0
    } else {
        (done as f64 / total as f64).clamp(0.0, 1.0)
    }
}

fn format_duration(duration: Duration) -> String {
    let secs = duration.as_secs();
    format!("{}:{:02}", secs / 60, secs % 60)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn track(id: &str, playlist: Option<&str>) -> Track {
        // Map short test names to valid 22-char base62 Spotify ids.
        let base62 = match id {
            "aaa" => "0000000000000000000000",
            "bbb" => "0000000000000000000001",
            "ccc" => "0000000000000000000002",
            other => panic!("unknown test track id: {other}"),
        };
        Track {
            id: SpotifyUri::from_uri(&format!("spotify:track:{base62}")).unwrap(),
            source_playlist: playlist.map(|name| name.to_string()),
        }
    }

    #[test]
    fn completing_a_shared_song_updates_every_playlist() {
        // "aaa" is in both playlists; "bbb" only in the first.
        let tracks = vec![
            track("aaa", Some("A")),
            track("bbb", Some("A")),
            track("aaa", Some("B")),
            track("ccc", Some("B")),
        ];
        let (answer_tx, answer_rx) = tokio_mpsc::channel(4);
        let ui = DownloadUi::new(&tracks, answer_tx, answer_rx);

        ui.track_started(&tracks[0]);
        ui.track_metadata("Song A".to_string(), 1024);
        ui.track_bytes(512);
        ui.track_completed(&tracks[0].id, true, 2048);

        ui.with_state(|state| {
            // Both playlists containing "aaa" advanced at once.
            assert_eq!(state.playlists[0].done, 1);
            assert_eq!(state.playlists[1].done, 1);
            assert_eq!(state.completed.len(), 1);
            assert_eq!(state.total_unique, 3);
            // Playlist totals count unique songs.
            assert_eq!(state.playlists[0].total, 2);
            assert_eq!(state.playlists[1].total, 2);
        });

        ui.track_completed(&tracks[1].id, true, 4096);
        ui.track_completed(&tracks[3].id, false, 0);
        ui.with_state(|state| {
            assert_eq!(state.playlists[0].done, 2);
            assert_eq!(state.playlists[1].done, 2);
        });
        assert_eq!(
            ui.playlists_of(&tracks[0].id),
            vec!["A".to_string(), "B".to_string()]
        );
    }

    #[test]
    fn summary_counts_downloads_skips_and_bytes() {
        let tracks = vec![
            track("aaa", Some("A")),
            track("bbb", Some("A")),
            track("ccc", None),
        ];
        let (answer_tx, answer_rx) = tokio_mpsc::channel(4);
        let ui = DownloadUi::new(&tracks, answer_tx, answer_rx);
        ui.playlist_synced("A");
        ui.track_completed(&tracks[0].id, true, 1024 * 1024);
        ui.track_completed(&tracks[1].id, false, 0);
        // "ccc" failed: never completed.

        let summary = ui.summary();
        assert_eq!(summary.playlists_synced, 1);
        assert_eq!(summary.titles_total, 3);
        assert_eq!(summary.downloaded, 1);
        assert_eq!(summary.skipped, 1);
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.new_bytes, 1024 * 1024);
        assert_eq!(
            summary.to_string(),
            "Synced 1 playlist, 3 title(s) total, 1 downloaded, 1 already on disk, 1 failed, 1.0 MB new on disk"
        );
    }

    #[test]
    fn duplicate_entries_in_one_playlist_count_once() {
        let tracks = vec![
            track("aaa", Some("A")),
            track("aaa", Some("A")),
            track("bbb", Some("A")),
        ];
        let (answer_tx, answer_rx) = tokio_mpsc::channel(4);
        let ui = DownloadUi::new(&tracks, answer_tx, answer_rx);
        ui.track_completed(&tracks[0].id, true, 0);
        ui.with_state(|state| {
            assert_eq!(state.playlists[0].total, 2);
            assert_eq!(state.playlists[0].done, 1);
        });
    }

    #[test]
    fn abort_and_finish_flags() {
        let tracks = vec![track("aaa", None)];
        let (answer_tx, answer_rx) = tokio_mpsc::channel(4);
        let ui = DownloadUi::new(&tracks, answer_tx, answer_rx);
        assert!(!ui.is_aborted());
        ui.abort();
        assert!(ui.is_aborted());
        ui.finish();
        ui.with_state(|state| assert!(state.finished));
    }

    #[tokio::test]
    async fn questions_are_answered_one_by_one() {
        let tracks = vec![track("aaa", Some("A"))];
        let (answer_tx, answer_rx) = tokio_mpsc::channel(4);
        let ui = DownloadUi::new(&tracks, answer_tx, answer_rx);

        assert!(ui.current_question().is_none());
        ui.push_question(PendingQuestion::SelectFolder);
        assert!(matches!(
            ui.current_question(),
            Some(PendingQuestion::SelectFolder)
        ));
        assert_eq!(ui.question_progress(), (0, 1));

        // The answer arrives on the channel the download loop consumes.
        ui.send_local_answer(PendingAnswer::Folder(PathBuf::from("/tmp/music")));
        match ui.recv_local_answer().await {
            Some(PendingAnswer::Folder(path)) => {
                assert_eq!(path, PathBuf::from("/tmp/music"));
            }
            other => panic!("unexpected answer: {other:?}"),
        }
        assert!(ui.current_question().is_none());
        assert_eq!(ui.question_progress(), (1, 1));

        // A track question with candidates.
        ui.push_question(PendingQuestion::MatchTrack {
            info: LocalTrackInfo {
                artist: "OSIVE x THNK PNK".to_string(),
                title: "CHOP SUEY (EDIT)".to_string(),
            },
            playlist: Some("A".to_string()),
            candidates: vec![PathBuf::from("/tmp/music/song.mp3")],
        });
        ui.send_local_answer(PendingAnswer::Match(Some(PathBuf::from(
            "/tmp/music/song.mp3",
        ))));
        match ui.recv_local_answer().await {
            Some(PendingAnswer::Match(Some(path))) => {
                assert_eq!(path, PathBuf::from("/tmp/music/song.mp3"));
            }
            other => panic!("unexpected answer: {other:?}"),
        }
        assert_eq!(ui.question_progress(), (2, 2));
    }

    #[test]
    fn renders_playlist_percentages_and_bars() {
        let tracks = vec![
            track("aaa", Some("Alpha")),
            track("bbb", Some("Alpha")),
            track("ccc", Some("Beta")),
        ];
        let (answer_tx, answer_rx) = tokio_mpsc::channel(4);
        let ui = DownloadUi::new(&tracks, answer_tx, answer_rx);
        ui.track_completed(&tracks[0].id, true, 0);
        ui.track_started(&tracks[1]);
        ui.track_metadata("Song B".to_string(), 3 * 1024 * 1024);
        ui.track_bytes(1024 * 1024);

        let mut terminal = Terminal::new(TestBackend::new(70, 14)).unwrap();
        terminal
            .draw(|frame| {
                draw(frame, &ui, None, &mut ListState::default(), None, 0, None)
            })
            .unwrap();
        let content = buffer_content(terminal.backend().buffer());

        assert!(content.contains("1/2 (50%)"));
        assert!(content.contains("0/1 (0%)"));
        assert!(content.contains("Overall 1/3 (33%)"));
        assert!(content.contains("Downloading: Song B (1/3 MB)"));
        assert!(content.contains("Log"));
    }

    #[test]
    fn renders_rate_limit_bar() {
        let tracks = vec![track("aaa", None)];
        let (answer_tx, answer_rx) = tokio_mpsc::channel(4);
        let ui = DownloadUi::new(&tracks, answer_tx, answer_rx);
        ui.finish();

        let limiter = PersistentRateLimiter::new(
            std::env::temp_dir().join("spotify-dl-test-rate-limit-state.json"),
            crate::rate_limit::RateLimitConfig {
                max_downloads: std::num::NonZeroU32::new(10).unwrap(),
                period: Duration::from_secs(60),
                report_interval: Duration::from_secs(30),
            },
        )
        .unwrap();

        let mut terminal = Terminal::new(TestBackend::new(70, 10)).unwrap();
        terminal
            .draw(|frame| {
                draw(
                    frame,
                    &ui,
                    Some(&limiter),
                    &mut ListState::default(),
                    None,
                    0,
                    None,
                )
            })
            .unwrap();
        let content = buffer_content(terminal.backend().buffer());
        assert!(content.contains("Rate limit 10/10"));
        assert!(content.contains("Overall 0/1 (0%)"));
        assert!(content.contains("Done"));
    }

    fn buffer_content(buffer: &ratatui::buffer::Buffer) -> String {
        let area = buffer.area;
        (0..area.height)
            .map(|y| {
                (0..area.width)
                    .filter_map(|x| buffer.cell((x, y)).map(|cell| cell.symbol().to_string()))
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}
