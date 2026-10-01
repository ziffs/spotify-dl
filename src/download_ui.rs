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
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::mpsc::TryRecvError;
use std::time::Duration;

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

use crate::log;
use crate::rate_limit::PersistentRateLimiter;
use crate::track::Track;

/// How often the download TUI redraws and polls for Ctrl-C.
const TICK: Duration = Duration::from_millis(200);

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
}

/// Shared handle between the download loop and the download TUI.
#[derive(Clone)]
pub struct DownloadUi {
    state: Arc<Mutex<State>>,
    abort: Arc<AtomicBool>,
}

impl DownloadUi {
    /// Builds the progress model from the track entries that are about to be
    /// downloaded. A song appearing in several playlists counts once per
    /// playlist, and once overall.
    pub fn new(tracks: &[Track]) -> Self {
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

    /// The song finished (downloaded, or already on disk). Every playlist that
    /// contains it gets its percentage updated.
    pub fn track_completed(&self, id: &SpotifyUri) {
        self.with_state(|state| {
            if state.completed.insert(id.clone())
                && let Some(indices) = state.track_playlists.get(id)
            {
                for &index in indices {
                    state.playlists[index].done += 1;
                }
            }
            state.current = None;
            state.current_playlist = None;
        });
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
    log::set_tui_active(false);
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
    let result = loop {
        if let Err(err) =
            terminal.draw(|frame| draw(frame, &ui, rate_limiter.as_deref(), &mut list_state))
        {
            break Err(anyhow!("download view failed: {err}"));
        }

        // Ctrl-C requests a graceful abort; other keys are ignored. A failing
        // event poll (no controlling terminal) is not fatal.
        match crossterm_event::poll(TICK) {
            Ok(true) => {
                if let Event::Key(key) = crossterm_event::read()?
                    && key.kind == KeyEventKind::Press
                    && key.modifiers.contains(KeyModifiers::CONTROL)
                    && key.code == KeyCode::Char('c')
                {
                    ui.abort();
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

fn draw(
    frame: &mut Frame,
    ui: &DownloadUi,
    rate_limiter: Option<&PersistentRateLimiter>,
    list_state: &mut ListState,
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

        let log_height = chunks[0].height.saturating_sub(2) as usize;
        let lines: Vec<Line> = log::recent_lines(log_height)
            .into_iter()
            .map(Line::raw)
            .collect();
        frame.render_widget(
            Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title("Log")),
            chunks[0],
        );

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
        let ui = DownloadUi::new(&tracks);

        ui.track_started(&tracks[0]);
        ui.track_metadata("Song A".to_string(), 1024);
        ui.track_bytes(512);
        ui.track_completed(&tracks[0].id);

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

        ui.track_completed(&tracks[1].id);
        ui.track_completed(&tracks[3].id);
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
    fn duplicate_entries_in_one_playlist_count_once() {
        let tracks = vec![
            track("aaa", Some("A")),
            track("aaa", Some("A")),
            track("bbb", Some("A")),
        ];
        let ui = DownloadUi::new(&tracks);
        ui.track_completed(&tracks[0].id);
        ui.with_state(|state| {
            assert_eq!(state.playlists[0].total, 2);
            assert_eq!(state.playlists[0].done, 1);
        });
    }

    #[test]
    fn abort_and_finish_flags() {
        let tracks = vec![track("aaa", None)];
        let ui = DownloadUi::new(&tracks);
        assert!(!ui.is_aborted());
        ui.abort();
        assert!(ui.is_aborted());
        ui.finish();
        ui.with_state(|state| assert!(state.finished));
    }

    #[test]
    fn renders_playlist_percentages_and_bars() {
        let tracks = vec![
            track("aaa", Some("Alpha")),
            track("bbb", Some("Alpha")),
            track("ccc", Some("Beta")),
        ];
        let ui = DownloadUi::new(&tracks);
        ui.track_completed(&tracks[0].id);
        ui.track_started(&tracks[1]);
        ui.track_metadata("Song B".to_string(), 3 * 1024 * 1024);
        ui.track_bytes(1024 * 1024);

        let mut terminal = Terminal::new(TestBackend::new(70, 14)).unwrap();
        terminal
            .draw(|frame| draw(frame, &ui, None, &mut ListState::default()))
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
        let ui = DownloadUi::new(&tracks);
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
            .draw(|frame| draw(frame, &ui, Some(&limiter), &mut ListState::default()))
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
