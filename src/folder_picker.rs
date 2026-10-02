use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::mpsc;
use std::sync::mpsc::TryRecvError;
use std::time::Duration;

use anyhow::Result;
use anyhow::anyhow;
use ratatui::crossterm::event as crossterm_event;
use ratatui::crossterm::event::Event;
use ratatui::crossterm::event::KeyCode;
use ratatui::crossterm::event::KeyEvent;
use ratatui::crossterm::event::KeyEventKind;
use ratatui::crossterm::event::KeyModifiers;
use ratatui::layout::Constraint;
use ratatui::layout::Layout;
use ratatui::style::Color;
use ratatui::style::Modifier;
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

use crate::account::Folder;
use crate::log;

/// How long to wait for a key event before redrawing.
/// A row of the tree view: either a folder (which toggles everything inside it)
/// or a single playlist.
#[derive(Debug, Clone)]
enum Row {
    Folder { id: usize, depth: usize },
    Playlist { uri: String, depth: usize },
}

impl Row {
    fn depth(&self) -> usize {
        match self {
            Row::Folder { depth, .. } | Row::Playlist { depth, .. } => *depth,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CheckState {
    All,
    Partial,
    None,
}

struct FolderInfo {
    name: String,
    /// Playlists directly inside the folder.
    playlists: Vec<String>,
    /// Every playlist inside the folder, including through nested folders.
    recursive_uris: Vec<String>,
    /// Ids of the nested folders, in display order.
    children: Vec<usize>,
}

/// What the picker wants to happen after the user leaves it.
pub enum Outcome {
    /// The user confirmed with Enter; contains the selected playlist URIs.
    Selected(Vec<String>),
    /// The user cancelled with Esc/q.
    Cancelled,
}

/// The kind of a visible row, for callers that inspect the tree view without
/// touching the terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    Folder,
    Playlist,
}

/// How often the picker redraws and polls for keys.
const TICK: Duration = Duration::from_millis(200);

/// Progress of the initial account metadata fetch, shown inside the picker
/// while it loads.
#[derive(Debug, Default, Clone)]
pub enum LoadingStage {
    /// Fetching the rootlist (the folder tree).
    #[default]
    Rootlist,
    /// Fetching the display name of every playlist.
    Names { done: usize, total: usize },
}

/// Shared slot the metadata fetch task reports its progress through.
pub type LoadingProgress = Arc<Mutex<LoadingStage>>;

/// Interactive tree view over the account's playlist folders.
///
/// The terminal interaction lives in [`run`]; the state transitions are kept as
/// plain methods so they can be exercised by unit tests.
pub struct FolderPicker {
    folders: HashMap<usize, FolderInfo>,
    top_level: Vec<usize>,
    root_playlists: Vec<String>,
    names: HashMap<String, String>,
    /// Playlists a download run has completed, by unix timestamp (seconds).
    downloaded: HashMap<String, u64>,
    rows: Vec<Row>,
    cursor: usize,
    collapsed: HashSet<usize>,
    checked: HashSet<String>,
    status: Option<String>,
    refresh_requested: bool,
    quit: bool,
    submit: bool,
}

impl FolderPicker {
    pub fn new(root: &Folder, names: HashMap<String, String>) -> Self {
        let mut picker = Self {
            folders: HashMap::new(),
            top_level: Vec::new(),
            root_playlists: Vec::new(),
            names,
            downloaded: HashMap::new(),
            rows: Vec::new(),
            cursor: 0,
            collapsed: HashSet::new(),
            checked: HashSet::new(),
            status: None,
            refresh_requested: false,
            quit: false,
            submit: false,
        };
        picker.register_tree(root);
        picker.rebuild_rows();
        picker
    }

    /// Pre-selects playlists from a previous run; URIs that no longer exist in
    /// the tree are dropped.
    pub fn with_pre_checked(mut self, uris: HashSet<String>) -> Self {
        let all = self.all_tree_uris();
        self.checked = uris.into_iter().filter(|uri| all.contains(uri)).collect();
        self
    }

    pub fn with_downloaded(mut self, downloaded: HashMap<String, u64>) -> Self {
        self.downloaded = downloaded;
        self
    }

    pub fn selected_count(&self) -> usize {
        self.checked.len()
    }

    /// The currently checked playlist URIs.
    pub fn checked(&self) -> &HashSet<String> {
        &self.checked
    }

    pub(crate) fn set_status(&mut self, status: Option<String>) {
        self.status = status;
    }

    /// Number of currently visible rows.
    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    /// The kind of the row at `index`, if visible.
    pub fn row_kind(&self, index: usize) -> Option<RowKind> {
        Some(match self.rows.get(index)? {
            Row::Folder { .. } => RowKind::Folder,
            Row::Playlist { .. } => RowKind::Playlist,
        })
    }

    /// Moves the cursor to `index`, clamped to the visible rows.
    pub fn set_cursor(&mut self, index: usize) {
        self.cursor = index.min(self.rows.len().saturating_sub(1));
    }

    /// The selected playlist URIs, in tree order.
    pub fn selected_uris(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        for uri in &self.root_playlists {
            if self.checked.contains(uri) && seen.insert(uri.clone()) {
                out.push(uri.clone());
            }
        }
        for &id in &self.top_level {
            collect_checked(&self.folders, &self.checked, &mut seen, &mut out, id);
        }
        out
    }

    pub(crate) fn should_submit(&self) -> bool {
        self.submit
    }

    pub(crate) fn should_quit(&self) -> bool {
        self.quit
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => self.quit = true,
            KeyCode::Up | KeyCode::Char('k') => self.move_up(),
            KeyCode::Down | KeyCode::Char('j') => self.move_down(),
            KeyCode::Home | KeyCode::Char('g') => self.move_home(),
            KeyCode::End | KeyCode::Char('G') => self.move_end(),
            KeyCode::Left | KeyCode::Char('h') => self.collapse_current(),
            KeyCode::Right | KeyCode::Char('l') => self.expand_current(),
            KeyCode::Char('r') | KeyCode::Char('R') => self.refresh_requested = true,
            KeyCode::Char(' ') => self.toggle_current(),
            KeyCode::Enter => self.confirm(),
            _ => {}
        }
    }

    pub fn move_up(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn move_down(&mut self) {
        if self.cursor + 1 < self.rows.len() {
            self.cursor += 1;
        }
    }

    pub fn move_home(&mut self) {
        self.cursor = 0;
    }

    pub fn move_end(&mut self) {
        self.cursor = self.rows.len().saturating_sub(1);
    }

    /// Space: toggles the highlighted playlist, or every playlist inside the
    /// highlighted folder (recursively).
    pub fn toggle_current(&mut self) {
        let Some(row) = self.rows.get(self.cursor).cloned() else {
            return;
        };
        match row {
            Row::Playlist { uri, .. } => {
                if !self.checked.remove(&uri) {
                    self.checked.insert(uri);
                }
                self.status = None;
            }
            Row::Folder { id, .. } => {
                let Some(info) = self.folders.get(&id) else {
                    return;
                };
                if info.recursive_uris.is_empty() {
                    self.status = Some("This folder contains no playlists".to_string());
                    return;
                }
                if info
                    .recursive_uris
                    .iter()
                    .all(|uri| self.checked.contains(uri))
                {
                    for uri in &info.recursive_uris {
                        self.checked.remove(uri);
                    }
                } else {
                    for uri in info.recursive_uris.clone() {
                        self.checked.insert(uri);
                    }
                }
                self.status = None;
            }
        }
    }

    /// Left: collapses the highlighted folder, or — when on a playlist — its
    /// parent folder.
    pub fn collapse_current(&mut self) {
        let Some(row) = self.rows.get(self.cursor).cloned() else {
            return;
        };
        match row {
            Row::Folder { id, .. } => {
                if self.collapsed.insert(id) {
                    self.rebuild_rows();
                }
            }
            Row::Playlist { .. } => {
                if let Some(index) = self.parent_folder_row(self.cursor) {
                    let Row::Folder { id, .. } = &self.rows[index] else {
                        return;
                    };
                    if self.collapsed.insert(*id) {
                        self.cursor = index;
                        self.rebuild_rows();
                    }
                }
            }
        }
    }

    /// Right: expands the highlighted folder.
    pub fn expand_current(&mut self) {
        let Some(Row::Folder { id, .. }) = self.rows.get(self.cursor) else {
            return;
        };
        if self.collapsed.remove(id) {
            self.rebuild_rows();
        }
    }

    fn confirm(&mut self) {
        if self.checked.is_empty() {
            self.status =
                Some("No playlists selected — press space on a playlist or folder".to_string());
        } else {
            self.submit = true;
        }
    }

    fn take_refresh(&mut self) -> bool {
        std::mem::take(&mut self.refresh_requested)
    }

    /// Replaces the tree and names after a metadata refresh, keeping the
    /// selection (restricted to playlists that still exist) and the downloaded
    /// status.
    pub(crate) fn update(&mut self, root: Folder, names: HashMap<String, String>) {
        self.register_tree(&root);
        self.names = names;
        self.collapsed.clear();
        self.status = None;
        let all = self.all_tree_uris();
        self.checked.retain(|uri| all.contains(uri));
        self.rebuild_rows();
    }

    fn all_tree_uris(&self) -> HashSet<String> {
        let mut out: HashSet<String> = self.root_playlists.iter().cloned().collect();
        for info in self.folders.values() {
            out.extend(info.playlists.iter().cloned());
        }
        out
    }

    /// Registers the folders of `root`, assigning fresh stable ids.
    fn register_tree(&mut self, root: &Folder) {
        let mut folders = HashMap::new();
        let mut next_id = 0usize;
        let mut top_level = Vec::new();
        for folder in &root.children {
            let id = register_folder(folder, &mut next_id, &mut folders);
            top_level.push(id);
        }
        self.folders = folders;
        self.top_level = top_level;
        self.root_playlists = dedupe(root.playlists.clone());
    }

    fn folder_state(&self, id: usize) -> CheckState {
        let Some(info) = self.folders.get(&id) else {
            return CheckState::None;
        };
        let checked = info
            .recursive_uris
            .iter()
            .filter(|uri| self.checked.contains(*uri))
            .count();
        if checked == 0 {
            CheckState::None
        } else if checked == info.recursive_uris.len() {
            CheckState::All
        } else {
            CheckState::Partial
        }
    }

    /// Index of the folder row that visually contains the row at `index`.
    fn parent_folder_row(&self, index: usize) -> Option<usize> {
        let depth = self.rows.get(index)?.depth();
        (0..index)
            .rev()
            .find(|&i| matches!(self.rows[i], Row::Folder { .. }) && self.rows[i].depth() < depth)
    }

    fn rebuild_rows(&mut self) {
        let mut rows = Vec::new();
        for uri in &self.root_playlists {
            rows.push(Row::Playlist {
                uri: uri.clone(),
                depth: 0,
            });
        }
        for &id in &self.top_level {
            push_folder_rows(&mut rows, &self.folders, &self.collapsed, id, 0);
        }
        self.rows = rows;
        if self.cursor >= self.rows.len() {
            self.cursor = self.rows.len().saturating_sub(1);
        }
    }

    fn display_name<'a>(&'a self, uri: &'a str) -> &'a str {
        match self.names.get(uri) {
            Some(name) => name,
            None => uri,
        }
    }

    fn list_item<'a>(&'a self, row: &'a Row) -> ListItem<'a> {
        match row {
            Row::Folder { id, depth } => {
                let info = &self.folders[id];
                let marker = if self.collapsed.contains(id) {
                    "▸ "
                } else {
                    "▾ "
                };
                let checkbox = match self.folder_state(*id) {
                    CheckState::All => Span::styled("[x] ", Style::default().fg(Color::Green)),
                    CheckState::Partial => Span::styled("[~] ", Style::default().fg(Color::Yellow)),
                    CheckState::None => Span::raw("[ ] "),
                };
                let count = info.recursive_uris.len();
                let downloaded = info
                    .recursive_uris
                    .iter()
                    .filter(|uri| self.downloaded.contains_key(*uri))
                    .count();
                let downloaded_note = if downloaded > 0 {
                    format!(", {downloaded} downloaded")
                } else {
                    String::new()
                };
                ListItem::new(Line::from(vec![
                    Span::raw("  ".repeat(*depth)),
                    Span::raw(marker),
                    checkbox,
                    Span::styled(
                        info.name.clone(),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(format!(
                        " ({} playlist{}{})",
                        count,
                        if count == 1 { "" } else { "s" },
                        downloaded_note
                    )),
                ]))
            }
            Row::Playlist { uri, depth } => {
                let checkbox = if self.checked.contains(uri) {
                    Span::styled("[x] ", Style::default().fg(Color::Green))
                } else {
                    Span::raw("[ ] ")
                };
                let mut spans = vec![
                    Span::raw("  ".repeat(*depth)),
                    checkbox,
                    Span::raw(self.display_name(uri)),
                ];
                if self.downloaded.contains_key(uri) {
                    spans.push(Span::styled(" ✓", Style::default().fg(Color::Green)));
                }
                ListItem::new(Line::from(spans))
            }
        }
    }
}

/// Shows the picker: first the metadata loading progress, then the tree view.
///
/// The account data arrives through `loaded` (fetched in the background);
/// `persist` is called whenever the selection changes, so an abort keeps the
/// selection for the next run. `refresh` re-fetches the account's tree and
/// playlist names (triggered with `R`); the picker is updated in place.
pub(crate) fn run(
    progress: LoadingProgress,
    mut loaded: mpsc::Receiver<Result<(Folder, HashMap<String, String>)>>,
    pre_checked: HashSet<String>,
    downloaded: HashMap<String, u64>,
    persist: &mut dyn FnMut(&HashSet<String>),
    refresh: &mut dyn FnMut() -> Result<(Folder, HashMap<String, String>)>,
) -> Result<Outcome> {
    log::set_tui_active(true);
    let mut terminal = ratatui::init();
    let result = event_loop(
        &mut terminal,
        progress,
        &mut loaded,
        pre_checked,
        downloaded,
        persist,
        refresh,
    );
    ratatui::restore();
    log::set_tui_active(false);
    result
}

fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    progress: LoadingProgress,
    loaded: &mut mpsc::Receiver<Result<(Folder, HashMap<String, String>)>>,
    pre_checked: HashSet<String>,
    downloaded: HashMap<String, u64>,
    persist: &mut dyn FnMut(&HashSet<String>),
    refresh: &mut dyn FnMut() -> Result<(Folder, HashMap<String, String>)>,
) -> Result<Outcome> {
    // `None` while the initial metadata is loading.
    let mut picker: Option<FolderPicker> = None;
    let mut saved_checked: Option<HashSet<String>> = None;
    let started = std::time::Instant::now();

    loop {
        terminal.draw(|frame| match &picker {
            Some(picker) => draw(frame, picker),
            None => draw_loading(frame, &progress, started),
        })?;

        // Keys: everything while the tree is shown; only quit while loading.
        // A failing event poll (no controlling terminal) is not fatal.
        let mut quit_requested = false;
        match crossterm_event::poll(TICK) {
            Ok(true) => {
                if let Event::Key(key) = crossterm_event::read()?
                    && key.kind == KeyEventKind::Press
                {
                    match &mut picker {
                        Some(active) => active.handle_key(key),
                        None => {
                            if matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
                                || (key.code == KeyCode::Char('c')
                                    && key.modifiers.contains(KeyModifiers::CONTROL))
                            {
                                quit_requested = true;
                            }
                        }
                    }
                }
            }
            Ok(false) => {}
            Err(_) => std::thread::sleep(TICK),
        }
        if quit_requested {
            return Ok(Outcome::Cancelled);
        }

        let Some(active) = picker.as_mut() else {
            // Still loading: watch for the account data to arrive.
            match loaded.try_recv() {
                Ok(Ok((root, names))) => {
                    let loaded_picker = FolderPicker::new(&root, names)
                        .with_pre_checked(pre_checked.clone())
                        .with_downloaded(downloaded.clone());
                    saved_checked = Some(loaded_picker.checked().clone());
                    picker = Some(loaded_picker);
                }
                Ok(Err(err)) => return Err(err),
                Err(TryRecvError::Disconnected) => {
                    return Err(anyhow!("the account metadata task ended unexpectedly"));
                }
                Err(TryRecvError::Empty) => {}
            }
            continue;
        };

        if active.should_submit() {
            return Ok(Outcome::Selected(active.selected_uris()));
        }
        if active.should_quit() {
            return Ok(Outcome::Cancelled);
        }
        if active.take_refresh() {
            active.set_status(Some("Refreshing metadata from account...".to_string()));
            terminal.draw(|frame| draw(frame, active))?;
            match refresh() {
                Ok((root, names)) => active.update(root, names),
                Err(err) => active.set_status(Some(format!("Refresh failed: {err:#}"))),
            }
        }
        if active.checked() != saved_checked.as_ref().unwrap_or(&HashSet::new()) {
            saved_checked = Some(active.checked().clone());
            persist(active.checked());
        }
    }
}

fn ratio(done: usize, total: usize) -> f64 {
    if total == 0 {
        0.0
    } else {
        (done as f64 / total as f64).clamp(0.0, 1.0)
    }
}

fn draw_loading(
    frame: &mut ratatui::Frame,
    progress: &LoadingProgress,
    started: std::time::Instant,
) {
    let area = frame.area();
    let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(area);
    let centered = Layout::vertical([
        Constraint::Percentage(40),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Percentage(40),
    ])
    .split(chunks[0]);

    let stage = progress
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();

    let spinner = SPINNER[(started.elapsed().as_millis() / 120) as usize % SPINNER.len()];
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(spinner.to_string(), Style::default().fg(Color::Cyan)),
            Span::raw(" Loading your playlists…"),
        ])),
        centered[1],
    );

    let (stage_line, gauge) = match &stage {
        LoadingStage::Rootlist => (
            Line::from("Fetching playlists from your account…"),
            Gauge::default()
                .label("Fetching playlists…")
                .gauge_style(Style::default().fg(Color::Cyan)),
        ),
        LoadingStage::Names { done, total } => (
            Line::from(format!("Fetching playlist names… {done}/{total}")),
            Gauge::default()
                .ratio(ratio(*done, *total))
                .label(format!("Playlist names {done}/{total}"))
                .gauge_style(Style::default().fg(Color::Cyan)),
        ),
    };
    frame.render_widget(Paragraph::new(stage_line), centered[2]);
    frame.render_widget(gauge, centered[3]);

    frame.render_widget(Paragraph::new(Line::from("esc cancel")), chunks[1]);
}

const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

fn draw(frame: &mut ratatui::Frame, picker: &FolderPicker) {
    let chunks = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .split(frame.area());

    frame.render_widget(header(picker), chunks[0]);

    let items: Vec<ListItem> = picker
        .rows
        .iter()
        .map(|row| picker.list_item(row))
        .collect();
    let mut list_state = ListState::default().with_selected(Some(picker.cursor));
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Your playlists"),
        )
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    frame.render_stateful_widget(list, chunks[1], &mut list_state);

    frame.render_widget(footer(), chunks[2]);
}

fn header(picker: &FolderPicker) -> Paragraph<'_> {
    let count = picker.selected_count();
    let mut second = vec![Span::raw(format!(
        "Selected: {} playlist{}",
        count,
        if count == 1 { "" } else { "s" }
    ))];
    if let Some(status) = &picker.status {
        second.push(Span::raw(" — "));
        second.push(Span::styled(
            status.clone(),
            Style::default().fg(Color::Yellow),
        ));
    }

    Paragraph::new(vec![
        Line::from(vec![
            Span::styled("spotify-dl", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" — select playlists to download"),
        ]),
        Line::from(second),
    ])
}

fn footer() -> Paragraph<'static> {
    fn key(name: &'static str) -> Span<'static> {
        Span::styled(name, Style::default().add_modifier(Modifier::BOLD))
    }

    Paragraph::new(Line::from(vec![
        key("↑/↓"),
        Span::raw(" move  "),
        key("←/→"),
        Span::raw(" fold/unfold  "),
        key("space"),
        Span::raw(" toggle  "),
        key("enter"),
        Span::raw(" download  "),
        key("esc"),
        Span::raw(" cancel"),
    ]))
}

/// Registers a folder (and its nested folders) in the picker, assigning stable
/// ids and precomputing the recursively contained playlists.
fn register_folder(
    folder: &Folder,
    next_id: &mut usize,
    folders: &mut HashMap<usize, FolderInfo>,
) -> usize {
    let id = *next_id;
    *next_id += 1;

    let mut recursive = folder.playlists.clone();
    let mut children = Vec::new();
    for child in &folder.children {
        let child_id = register_folder(child, next_id, folders);
        recursive.extend(folders[&child_id].recursive_uris.iter().cloned());
        children.push(child_id);
    }

    folders.insert(
        id,
        FolderInfo {
            name: folder.name.clone(),
            playlists: dedupe(folder.playlists.clone()),
            recursive_uris: dedupe(recursive),
            children,
        },
    );
    id
}

fn push_folder_rows(
    rows: &mut Vec<Row>,
    folders: &HashMap<usize, FolderInfo>,
    collapsed: &HashSet<usize>,
    id: usize,
    depth: usize,
) {
    let Some(info) = folders.get(&id) else {
        return;
    };
    rows.push(Row::Folder { id, depth });
    if collapsed.contains(&id) {
        return;
    }
    for uri in &info.playlists {
        rows.push(Row::Playlist {
            uri: uri.clone(),
            depth: depth + 1,
        });
    }
    for &child in &info.children {
        push_folder_rows(rows, folders, collapsed, child, depth + 1);
    }
}

fn collect_checked(
    folders: &HashMap<usize, FolderInfo>,
    checked: &HashSet<String>,
    seen: &mut HashSet<String>,
    out: &mut Vec<String>,
    id: usize,
) {
    let Some(info) = folders.get(&id) else {
        return;
    };
    for uri in &info.playlists {
        if checked.contains(uri) && seen.insert(uri.clone()) {
            out.push(uri.clone());
        }
    }
    for &child in &info.children {
        collect_checked(folders, checked, seen, out, child);
    }
}

fn dedupe(uris: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::with_capacity(uris.len());
    uris.into_iter()
        .filter(|uri| seen.insert(uri.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn folder(name: &str, playlists: &[&str], children: Vec<Folder>) -> Folder {
        Folder {
            name: name.to_string(),
            playlists: playlists.iter().map(|uri| uri.to_string()).collect(),
            children,
        }
    }

    fn picker() -> FolderPicker {
        let root = folder(
            "",
            &["spotify:playlist:loose"],
            vec![
                folder(
                    "Work",
                    &["spotify:playlist:work1"],
                    vec![folder(
                        "Gym",
                        &["spotify:playlist:gym1", "spotify:playlist:gym2"],
                        vec![],
                    )],
                ),
                folder("Empty", &[], vec![]),
            ],
        );
        FolderPicker::new(&root, HashMap::new())
    }

    /// The default tree renders these rows, in order.
    fn expected_rows() -> Vec<(String, usize)> {
        vec![
            ("playlist:spotify:playlist:loose".to_string(), 0),
            ("folder:Work".to_string(), 0),
            ("playlist:spotify:playlist:work1".to_string(), 1),
            ("folder:Gym".to_string(), 1),
            ("playlist:spotify:playlist:gym1".to_string(), 2),
            ("playlist:spotify:playlist:gym2".to_string(), 2),
            ("folder:Empty".to_string(), 0),
        ]
    }

    fn row_key(picker: &FolderPicker, index: usize) -> String {
        match &picker.rows[index] {
            Row::Folder { id, .. } => format!("folder:{}", picker.folders[id].name),
            Row::Playlist { uri, .. } => format!("playlist:{uri}"),
        }
    }

    fn row_depth(picker: &FolderPicker, index: usize) -> usize {
        picker.rows[index].depth()
    }

    fn cursor_key(picker: &FolderPicker) -> String {
        row_key(picker, picker.cursor)
    }

    #[test]
    fn builds_rows_for_root_playlists_and_folders() {
        let picker = picker();

        let actual: Vec<(String, usize)> = (0..picker.rows.len())
            .map(|i| (row_key(&picker, i), row_depth(&picker, i)))
            .collect();
        assert_eq!(actual, expected_rows());
        assert_eq!(picker.cursor, 0);
    }

    #[test]
    fn toggling_playlist_checks_it_individually() {
        let mut picker = picker();
        // Move to the first playlist inside "Work" (row 2).
        picker.cursor = 2;
        picker.toggle_current();

        assert_eq!(picker.selected_count(), 1);
        assert_eq!(picker.selected_uris(), vec!["spotify:playlist:work1"]);
        assert_eq!(picker.folder_state(0), CheckState::Partial);

        picker.toggle_current();
        assert_eq!(picker.selected_count(), 0);
        assert_eq!(picker.folder_state(0), CheckState::None);
    }

    #[test]
    fn toggling_folder_selects_everything_inside_recursively() {
        let mut picker = picker();
        // "Work" folder row.
        picker.cursor = 1;
        picker.toggle_current();

        assert_eq!(
            picker.selected_uris(),
            vec![
                "spotify:playlist:work1",
                "spotify:playlist:gym1",
                "spotify:playlist:gym2"
            ]
        );
        assert_eq!(picker.folder_state(0), CheckState::All);
        assert_eq!(picker.folder_state(1), CheckState::All);

        // Toggling again clears the whole folder.
        picker.toggle_current();
        assert_eq!(picker.selected_count(), 0);
        assert_eq!(picker.folder_state(0), CheckState::None);
    }

    #[test]
    fn partially_selected_folder_shows_partial_state() {
        let mut picker = picker();
        // First playlist inside "Gym" (row 4).
        picker.cursor = 4;
        picker.toggle_current();

        assert_eq!(picker.folder_state(1), CheckState::Partial);
        assert_eq!(picker.folder_state(0), CheckState::Partial);

        // Toggling the outer folder completes it (all descendants selected).
        picker.cursor = 1;
        picker.toggle_current();
        assert_eq!(picker.folder_state(0), CheckState::All);
        assert_eq!(picker.selected_count(), 3);
    }

    #[test]
    fn toggling_empty_folder_reports_status() {
        let mut picker = picker();
        // "Empty" folder row.
        picker.cursor = 6;
        picker.toggle_current();

        assert_eq!(picker.selected_count(), 0);
        assert_eq!(
            picker.status.as_deref(),
            Some("This folder contains no playlists")
        );
    }

    #[test]
    fn collapsing_folder_hides_subtree() {
        let mut picker = picker();
        // Collapse "Work" (row 1).
        picker.cursor = 1;
        picker.collapse_current();

        let actual: Vec<String> = (0..picker.rows.len())
            .map(|i| row_key(&picker, i))
            .collect();
        assert_eq!(
            actual,
            vec![
                "playlist:spotify:playlist:loose",
                "folder:Work",
                "folder:Empty",
            ]
        );
        // The cursor stays on the collapsed folder.
        assert_eq!(cursor_key(&picker), "folder:Work");

        // Expanding restores the subtree.
        picker.expand_current();
        let actual: Vec<(String, usize)> = (0..picker.rows.len())
            .map(|i| (row_key(&picker, i), row_depth(&picker, i)))
            .collect();
        assert_eq!(actual, expected_rows());
    }

    #[test]
    fn collapsing_from_a_playlist_targets_the_parent_folder() {
        let mut picker = picker();
        // Cursor on "gym1" (row 4); pressing left collapses its parent "Gym".
        picker.cursor = 4;
        picker.collapse_current();

        let actual: Vec<String> = (0..picker.rows.len())
            .map(|i| row_key(&picker, i))
            .collect();
        assert_eq!(
            actual,
            vec![
                "playlist:spotify:playlist:loose",
                "folder:Work",
                "playlist:spotify:playlist:work1",
                "folder:Gym",
                "folder:Empty",
            ]
        );
        assert_eq!(cursor_key(&picker), "folder:Gym");
    }

    #[test]
    fn selection_survives_collapsing() {
        let mut picker = picker();
        // Select the whole "Work" folder, then collapse it.
        picker.cursor = 1;
        picker.toggle_current();
        picker.collapse_current();

        assert_eq!(
            picker.selected_uris(),
            vec![
                "spotify:playlist:work1",
                "spotify:playlist:gym1",
                "spotify:playlist:gym2"
            ]
        );
    }

    #[test]
    fn selected_uris_follow_tree_order_and_dedupe() {
        let root = folder(
            "",
            &["spotify:playlist:dup"],
            vec![
                folder(
                    "A",
                    &["spotify:playlist:dup", "spotify:playlist:a1"],
                    vec![],
                ),
                folder("B", &["spotify:playlist:b1"], vec![]),
            ],
        );
        let mut picker = FolderPicker::new(&root, HashMap::new());

        // Select the two folders. "dup" is both a loose playlist and inside
        // "A", but must appear exactly once, in tree order.
        picker.cursor = 1; // "A"
        picker.toggle_current();
        picker.cursor = 4; // "B"
        picker.toggle_current();

        assert_eq!(picker.selected_count(), 3);
        assert_eq!(
            picker.selected_uris(),
            vec![
                "spotify:playlist:dup",
                "spotify:playlist:a1",
                "spotify:playlist:b1",
            ]
        );
    }

    #[test]
    fn confirm_without_selection_sets_status_instead_of_submitting() {
        let mut picker = picker();
        picker.confirm();

        assert!(!picker.should_submit());
        assert!(picker.status.is_some());

        picker.cursor = 0;
        picker.toggle_current();
        picker.confirm();
        assert!(picker.should_submit());
    }

    #[test]
    fn cursor_movement_is_clamped() {
        let mut picker = picker();
        picker.move_up();
        assert_eq!(picker.cursor, 0);

        picker.move_end();
        assert_eq!(picker.cursor, picker.rows.len() - 1);

        picker.move_down();
        assert_eq!(picker.cursor, picker.rows.len() - 1);

        picker.move_home();
        assert_eq!(picker.cursor, 0);
    }

    #[test]
    fn quit_and_navigation_keys_are_handled() {
        let mut picker = picker();
        picker.handle_key(KeyEvent::new(
            KeyCode::Down,
            crossterm_event::KeyModifiers::empty(),
        ));
        assert_eq!(cursor_key(&picker), "folder:Work");

        picker.handle_key(KeyEvent::new(
            KeyCode::Esc,
            crossterm_event::KeyModifiers::empty(),
        ));
        assert!(picker.should_quit());
    }

    #[test]
    fn release_key_events_are_ignored() {
        let mut picker = picker();
        let mut key = KeyEvent::new(KeyCode::Down, crossterm_event::KeyModifiers::empty());
        key.kind = KeyEventKind::Release;
        picker.handle_key(key);
        assert_eq!(picker.cursor, 0);
    }

    #[test]
    fn pre_checked_uris_are_selected_on_start() {
        let picker = picker().with_pre_checked(HashSet::from([
            "spotify:playlist:work1".to_string(),
            "spotify:playlist:gone".to_string(),
        ]));

        // "gone" no longer exists in the tree and is dropped.
        assert_eq!(picker.selected_count(), 1);
        assert_eq!(picker.selected_uris(), vec!["spotify:playlist:work1"]);
        assert_eq!(picker.folder_state(0), CheckState::Partial);
    }

    #[test]
    fn refresh_key_requests_a_refresh() {
        let mut picker = picker();
        picker.handle_key(KeyEvent::new(
            KeyCode::Char('r'),
            crossterm_event::KeyModifiers::empty(),
        ));
        assert!(picker.take_refresh());
        // The flag is consumed once.
        assert!(!picker.take_refresh());
    }

    #[test]
    fn update_keeps_selection_that_still_exists() {
        let mut picker = picker();
        picker.cursor = 1; // "Work"
        picker.toggle_current();
        assert_eq!(picker.selected_count(), 3);

        // After a refresh, Gym and its playlists are gone from the account.
        let new_root = folder(
            "",
            &["spotify:playlist:loose"],
            vec![folder(
                "Work",
                &["spotify:playlist:work1", "spotify:playlist:new1"],
                vec![],
            )],
        );
        picker.update(new_root, HashMap::new());

        assert_eq!(picker.selected_uris(), vec!["spotify:playlist:work1"]);
        let keys: Vec<String> = (0..picker.rows.len())
            .map(|i| row_key(&picker, i))
            .collect();
        assert_eq!(
            keys,
            vec![
                "playlist:spotify:playlist:loose",
                "folder:Work",
                "playlist:spotify:playlist:work1",
                "playlist:spotify:playlist:new1",
            ]
        );
    }

    #[test]
    fn downloaded_status_is_rendered() {
        let mut downloaded = HashMap::new();
        downloaded.insert("spotify:playlist:work1".to_string(), 42);
        let picker = picker().with_downloaded(downloaded);

        let mut terminal = Terminal::new(TestBackend::new(70, 14)).unwrap();
        terminal.draw(|frame| draw(frame, &picker)).unwrap();
        let content = buffer_content(terminal.backend().buffer());

        assert!(content.contains("[ ] spotify:playlist:work1 ✓"));
        assert!(content.contains("▾ [ ] Work (3 playlists, 1 downloaded)"));
        assert!(content.contains("[ ] spotify:playlist:loose"));
        assert!(!content.contains("loose ✓"));
    }

    #[test]
    fn renders_checkboxes_and_tree_structure() {
        let mut picker = picker();
        picker.cursor = 1; // highlight "Work"
        picker.toggle_current(); // select the whole folder

        let mut terminal = Terminal::new(TestBackend::new(60, 12)).unwrap();
        terminal.draw(|frame| draw(frame, &picker)).unwrap();

        let content = buffer_content(terminal.backend().buffer());
        assert!(content.contains("spotify-dl — select playlists to download"));
        assert!(content.contains("Selected: 3 playlists"));
        assert!(content.contains("▾ [x] Work (3 playlists)"));
        assert!(content.contains("[x] spotify:playlist:gym1"));
        assert!(content.contains("[ ] spotify:playlist:loose"));
        assert!(content.contains("▾ [ ] Empty (0 playlists)"));
        assert!(content.contains("space toggle"));
    }

    #[test]
    fn renders_loading_screen_with_progress() {
        let progress: LoadingProgress = Default::default();
        *progress.lock().unwrap() = LoadingStage::Names {
            done: 42,
            total: 129,
        };
        let started = std::time::Instant::now();

        let mut terminal = Terminal::new(TestBackend::new(60, 12)).unwrap();
        terminal
            .draw(|frame| draw_loading(frame, &progress, started))
            .unwrap();
        let content = buffer_content(terminal.backend().buffer());

        assert!(content.contains("Loading your playlists…"));
        assert!(content.contains("Fetching playlist names… 42/129"));
        assert!(content.contains("Playlist names 42/129"));
        assert!(content.contains("esc cancel"));
    }

    #[test]
    fn renders_rootlist_loading_stage() {
        let progress: LoadingProgress = Default::default();
        let started = std::time::Instant::now();

        let mut terminal = Terminal::new(TestBackend::new(60, 12)).unwrap();
        terminal
            .draw(|frame| draw_loading(frame, &progress, started))
            .unwrap();
        let content = buffer_content(terminal.backend().buffer());

        assert!(content.contains("Fetching playlists from your account…"));
        assert!(content.contains("Fetching playlists…"));
    }

    #[test]
    fn renders_playlist_names_when_known() {
        let root = folder("Work", &["spotify:playlist:work1"], vec![]);
        let mut names = HashMap::new();
        names.insert(
            "spotify:playlist:work1".to_string(),
            "Daily Mix".to_string(),
        );
        let picker = FolderPicker::new(&root, names);

        let mut terminal = Terminal::new(TestBackend::new(60, 12)).unwrap();
        terminal.draw(|frame| draw(frame, &picker)).unwrap();

        let content = buffer_content(terminal.backend().buffer());
        assert!(content.contains("[ ] Daily Mix"));
        assert!(!content.contains("spotify:playlist:work1"));
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
