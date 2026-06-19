use std::{
    cmp,
    collections::{HashMap, HashSet},
    env,
    future::Future,
    io::{self, Stdout},
    path::PathBuf,
    sync::mpsc::{self, Receiver, TryRecvError},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;
use crossterm::{
    cursor::MoveTo,
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{
        Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
        enable_raw_mode,
    },
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    prelude::*,
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
};
use rmux_proto::{
    DisplayMessageRequest, KillSessionRequest, KillWindowRequest, ListWindowsRequest,
    RenameSessionRequest, RenameWindowRequest, Request, Response, SelectWindowRequest, SessionName,
    SwitchClientExt3Request, Target, WindowListEntry, WindowTarget,
};
use rmux_sdk::{PaneId, Rmux, RmuxEndpoint};

const SEP: char = '\u{241F}';
const CURRENT_FORMAT: &str = "#{session_id}\u{241F}#{window_id}";

#[derive(Debug, Parser)]
#[command(author, version, about = "Fast RMUX session/window picker")]
struct Cli {
    /// Show one row per RMUX window instead of one row per session.
    #[arg(short = 'w', long)]
    all_windows: bool,

    /// Shortcut preset: show all windows with the last N lines in the preview pane.
    #[arg(long, value_name = "LINES")]
    window_lines: Option<usize>,

    /// Number of captured lines to show in the preview pane.
    #[arg(short = 'n', long, default_value_t = 10)]
    preview_lines: usize,

    /// Number of preview lines embedded into each row in the left list.
    #[arg(long, default_value_t = 0)]
    inline_lines: usize,

    /// Seconds between automatic refreshes while the picker is open. Use 0 to disable.
    #[arg(long, default_value_t = 5)]
    refresh_seconds: u64,

    /// Number of pane captures taken at startup to detect which windows are still
    /// producing output. 1 disables activity detection at startup; the marker
    /// then only becomes accurate after the first auto-refresh.
    #[arg(long, default_value_t = DEFAULT_ACTIVITY_SAMPLES)]
    activity_samples: usize,

    /// Milliseconds between activity samples at startup.
    #[arg(long, default_value_t = DEFAULT_ACTIVITY_INTERVAL_MS)]
    activity_interval_ms: u64,

    /// Print targets and exit without opening the picker.
    #[arg(long)]
    list: bool,
}

#[derive(Debug, Clone)]
struct Options {
    all_windows: bool,
    preview_lines: usize,
    inline_lines: usize,
    refresh_seconds: u64,
    activity_samples: usize,
    activity_interval: Duration,
}

impl Cli {
    fn options(&self) -> Options {
        let activity_interval = Duration::from_millis(self.activity_interval_ms);
        match self.window_lines {
            Some(lines) => Options {
                all_windows: true,
                preview_lines: lines,
                inline_lines: 0,
                refresh_seconds: self.refresh_seconds,
                activity_samples: self.activity_samples,
                activity_interval,
            },
            None => Options {
                all_windows: self.all_windows,
                preview_lines: self.preview_lines,
                inline_lines: self.inline_lines,
                refresh_seconds: self.refresh_seconds,
                activity_samples: self.activity_samples,
                activity_interval,
            },
        }
    }
}

#[derive(Debug, Clone)]
struct Entry {
    session_id: String,
    session_name: String,
    session_attached: bool,
    session_activity: u64,
    window_id: String,
    window_index: String,
    window_index_num: u32,
    window_name: String,
    window_active: bool,
    pane_id: String,
    pane_id_num: u32,
    pane_current_path: String,
    preview: Vec<String>,
    activity: Activity,
    activity_fingerprint: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Activity {
    #[default]
    Unknown,
    Idle,
    Active,
}

const DEFAULT_ACTIVITY_SAMPLES: usize = 4;
const DEFAULT_ACTIVITY_INTERVAL_MS: u64 = 300;

#[derive(Debug, Clone, Default)]
struct CurrentTarget {
    session_id: Option<String>,
    window_id: Option<String>,
}

#[derive(Debug, Clone)]
struct SessionGroup {
    id: String,
    name: String,
    attached: bool,
    activity: u64,
    /// Indices into `App::entries`, sorted by RMUX window index ascending.
    window_indices: Vec<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RenameKind {
    Session,
    Window,
}

/// In-progress inline rename of the selected session or window.
#[derive(Debug, Clone)]
struct RenameState {
    kind: RenameKind,
    /// session id (for Session) or window id (for Window).
    target_id: String,
    original: String,
    buffer: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KillKind {
    Session,
    Window,
}

/// A pending, not-yet-confirmed kill of the selected session or window.
#[derive(Debug, Clone)]
struct KillState {
    kind: KillKind,
    /// session id (for Session) or window id (for Window).
    target_id: String,
    label: String,
}

struct App {
    entries: Vec<Entry>,
    sessions: Vec<SessionGroup>,
    selected_session: usize,
    selected_window: usize,
    preview_lines: usize,
    inline_lines: usize,
    refresh_seconds: u64,
    current: CurrentTarget,
    status: Option<String>,
    probe: Option<Receiver<ActivityProbeResult>>,
    rename: Option<RenameState>,
    confirm_kill: Option<KillState>,
}

impl App {
    fn load(options: &Options) -> Result<Self> {
        let current = current_target();
        let entries = load_entries(true, options.preview_lines)?;
        let probe = spawn_activity_probe(&entries, &options);
        let sessions = group_sessions(&entries);
        let (selected_session, selected_window) = initial_position(&sessions, &entries, &current);
        Ok(Self {
            entries,
            sessions,
            selected_session,
            selected_window,
            preview_lines: options.preview_lines,
            inline_lines: options.inline_lines,
            refresh_seconds: options.refresh_seconds,
            current,
            status: None,
            probe,
            rename: None,
            confirm_kill: None,
        })
    }

    /// Drain any completed background activity probe and merge its result
    /// into our entries. Called on every loop iteration; cheap when the
    /// probe is still running or already drained.
    fn poll_probe(&mut self) -> bool {
        let Some(rx) = &self.probe else {
            return false;
        };
        match rx.try_recv() {
            Ok(result) => {
                for entry in &mut self.entries {
                    if let Some(activity) = result.activity.get(&entry.pane_id) {
                        entry.activity = *activity;
                    }
                    if let Some(fp) = result.fingerprints.get(&entry.pane_id) {
                        entry.activity_fingerprint = fp.clone();
                    }
                }
                self.probe = None;
                true
            }
            Err(TryRecvError::Empty) => false,
            Err(TryRecvError::Disconnected) => {
                self.probe = None;
                false
            }
        }
    }

    fn refresh(&mut self) {
        self.refresh_with_status("refreshed");
    }

    fn auto_refresh(&mut self) {
        self.refresh_with_status("auto refreshed");
    }

    fn refresh_with_status(&mut self, status: &str) {
        // Discard any in-flight startup probe — refresh data is fresher and the
        // probe's fingerprints would clobber the post-refresh state.
        self.probe = None;
        let prev_session_id = self
            .sessions
            .get(self.selected_session)
            .map(|s| s.id.clone());
        let prev_window_id = self.selected_entry().map(|e| e.window_id.clone());
        let prev_fingerprints: HashMap<String, String> = self
            .entries
            .iter()
            .map(|e| (e.pane_id.clone(), e.activity_fingerprint.clone()))
            .collect();

        match load_entries(true, self.preview_lines) {
            Ok(mut entries) => {
                apply_activity(&mut entries, &prev_fingerprints);
                let sessions = group_sessions(&entries);
                self.entries = entries;
                self.sessions = sessions;
                self.selected_session = prev_session_id
                    .as_deref()
                    .and_then(|id| self.sessions.iter().position(|s| s.id == id))
                    .unwrap_or(0);
                if self.selected_session >= self.sessions.len() {
                    self.selected_session = self.sessions.len().saturating_sub(1);
                }
                self.selected_window = self
                    .sessions
                    .get(self.selected_session)
                    .and_then(|s| {
                        prev_window_id.as_deref().and_then(|wid| {
                            s.window_indices
                                .iter()
                                .position(|&i| self.entries[i].window_id == wid)
                        })
                    })
                    .unwrap_or(0);
                self.current = current_target();
                self.status = Some(status.to_string());
            }
            Err(err) => {
                self.status = Some(format!("refresh failed: {err:#}"));
            }
        }
    }

    fn current_session(&self) -> Option<&SessionGroup> {
        self.sessions.get(self.selected_session)
    }

    fn next_window(&mut self) {
        let Some(session) = self.current_session() else {
            return;
        };
        let len = session.window_indices.len();
        if len == 0 {
            return;
        }
        self.selected_window = (self.selected_window + 1) % len;
    }

    fn previous_window(&mut self) {
        let Some(session) = self.current_session() else {
            return;
        };
        let len = session.window_indices.len();
        if len == 0 {
            return;
        }
        self.selected_window = (self.selected_window + len - 1) % len;
    }

    fn next_session(&mut self) {
        if self.sessions.is_empty() {
            return;
        }
        self.selected_session = cmp::min(self.selected_session + 1, self.sessions.len() - 1);
        self.selected_window = self.preferred_window_in_current_session();
    }

    fn previous_session(&mut self) {
        if self.sessions.is_empty() {
            return;
        }
        self.selected_session = self.selected_session.saturating_sub(1);
        self.selected_window = self.preferred_window_in_current_session();
    }

    fn preferred_window_in_current_session(&self) -> usize {
        self.current_session()
            .and_then(|s| {
                s.window_indices
                    .iter()
                    .position(|&i| self.entries[i].window_active)
            })
            .unwrap_or(0)
    }

    fn selected_entry(&self) -> Option<&Entry> {
        let session = self.current_session()?;
        let idx = *session.window_indices.get(self.selected_window)?;
        self.entries.get(idx)
    }

    fn begin_rename_session(&mut self) {
        if let Some(session) = self.current_session() {
            self.rename = Some(RenameState {
                kind: RenameKind::Session,
                target_id: session.name.clone(),
                original: session.name.clone(),
                buffer: session.name.clone(),
            });
        }
    }

    fn begin_rename_window(&mut self) {
        if let Some(entry) = self.selected_entry() {
            self.rename = Some(RenameState {
                kind: RenameKind::Window,
                target_id: window_target_string(entry),
                original: entry.window_name.clone(),
                buffer: entry.window_name.clone(),
            });
        }
    }

    fn rename_input(&mut self, c: char) {
        if let Some(rename) = &mut self.rename {
            rename.buffer.push(c);
        }
    }

    fn rename_backspace(&mut self) {
        if let Some(rename) = &mut self.rename {
            rename.buffer.pop();
        }
    }

    fn cancel_rename(&mut self) {
        if self.rename.take().is_some() {
            self.status = Some("rename cancelled".to_string());
        }
    }

    fn commit_rename(&mut self) {
        let Some(rename) = self.rename.take() else {
            return;
        };
        let new_name = rename.buffer.trim();
        if new_name.is_empty() || new_name == rename.original {
            self.status = Some("rename cancelled".to_string());
            return;
        }
        let result = match rename.kind {
            RenameKind::Session => rename_session(&rename.target_id, new_name),
            RenameKind::Window => rename_window(&rename.target_id, new_name),
        };
        match result {
            Ok(()) => {
                let what = match rename.kind {
                    RenameKind::Session => "session",
                    RenameKind::Window => "window",
                };
                self.refresh_with_status(&format!("renamed {what} to {new_name}"));
            }
            Err(err) => {
                self.status = Some(format!("rename failed: {err:#}"));
            }
        }
    }

    fn begin_kill_window(&mut self) {
        if let Some(entry) = self.selected_entry() {
            self.confirm_kill = Some(KillState {
                kind: KillKind::Window,
                target_id: window_target_string(entry),
                label: format!("{}: {}", entry.window_index, entry.window_name),
            });
        }
    }

    fn begin_kill_session(&mut self) {
        if let Some(session) = self.current_session() {
            self.confirm_kill = Some(KillState {
                kind: KillKind::Session,
                target_id: session.name.clone(),
                label: session.name.clone(),
            });
        }
    }

    fn cancel_kill(&mut self) {
        if self.confirm_kill.take().is_some() {
            self.status = Some("kill cancelled".to_string());
        }
    }

    fn commit_kill(&mut self) {
        let Some(kill) = self.confirm_kill.take() else {
            return;
        };
        let result = match kill.kind {
            KillKind::Session => kill_session(&kill.target_id),
            KillKind::Window => kill_window(&kill.target_id),
        };
        match result {
            Ok(()) => {
                let what = match kill.kind {
                    KillKind::Session => "session",
                    KillKind::Window => "window",
                };
                self.refresh_with_status(&format!("killed {what} {}", kill.label));
            }
            Err(err) => {
                self.status = Some(format!("kill failed: {err:#}"));
            }
        }
    }
}

fn group_sessions(entries: &[Entry]) -> Vec<SessionGroup> {
    let mut map: HashMap<String, SessionGroup> = HashMap::new();
    for (i, entry) in entries.iter().enumerate() {
        let group = map
            .entry(entry.session_id.clone())
            .or_insert_with(|| SessionGroup {
                id: entry.session_id.clone(),
                name: entry.session_name.clone(),
                attached: entry.session_attached,
                activity: entry.session_activity,
                window_indices: Vec::new(),
            });
        group.window_indices.push(i);
    }

    let mut sessions: Vec<SessionGroup> = map.into_values().collect();
    for session in &mut sessions {
        session
            .window_indices
            .sort_by_key(|&i| entries[i].window_index.parse::<u32>().unwrap_or(u32::MAX));
    }
    sessions.sort_by(|a, b| {
        b.attached
            .cmp(&a.attached)
            .then(b.activity.cmp(&a.activity))
            .then(a.name.cmp(&b.name))
    });
    sessions
}

fn initial_position(
    sessions: &[SessionGroup],
    entries: &[Entry],
    current: &CurrentTarget,
) -> (usize, usize) {
    let session_idx = current
        .session_id
        .as_deref()
        .and_then(|id| sessions.iter().position(|s| s.id == id))
        .unwrap_or(0);
    let window_idx = sessions
        .get(session_idx)
        .and_then(|session| {
            current
                .window_id
                .as_deref()
                .and_then(|wid| {
                    session
                        .window_indices
                        .iter()
                        .position(|&i| entries[i].window_id == wid)
                })
                .or_else(|| {
                    session
                        .window_indices
                        .iter()
                        .position(|&i| entries[i].window_active)
                })
        })
        .unwrap_or(0);
    (session_idx, window_idx)
}

struct Tui {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl Tui {
    fn new() -> Result<Self> {
        enable_raw_mode().context("enable terminal raw mode")?;
        let mut stdout = io::stdout();
        execute!(
            stdout,
            EnterAlternateScreen,
            Clear(ClearType::All),
            MoveTo(0, 0)
        )
        .context("enter alternate screen")?;
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend).context("create terminal backend")?;
        Ok(Self { terminal })
    }

    fn draw(&mut self, app: &App) -> Result<()> {
        self.terminal
            .draw(|frame| render(frame, app))
            .context("draw terminal UI")?;
        Ok(())
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        let _ = self.terminal.show_cursor();
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let options = cli.options();
    if cli.list {
        print_targets(&options)?;
        return Ok(());
    }

    let target = run_picker(&options)?;

    if let Some(entry) = target {
        switch_to(&entry)?;
    }

    Ok(())
}

fn print_targets(options: &Options) -> Result<()> {
    let entries = load_entries(options.all_windows, options.preview_lines)?;
    for entry in entries {
        let lines = tail_non_empty(&entry.preview, cmp::max(options.inline_lines, 1));
        if options.inline_lines <= 1 {
            let last_line = lines
                .last()
                .cloned()
                .unwrap_or_else(|| "<no output>".to_string());
            println!(
                "{}\t{}:{}\t{}\t{}",
                entry.session_name,
                entry.window_index,
                entry.window_name,
                entry.pane_current_path,
                last_line
            );
        } else {
            println!(
                "{}\t{}:{}\t{}",
                entry.session_name, entry.window_index, entry.window_name, entry.pane_current_path
            );
            if lines.is_empty() {
                println!("  <no output>");
            } else {
                for line in lines {
                    println!("  {line}");
                }
            }
        }
    }
    Ok(())
}

fn run_picker(options: &Options) -> Result<Option<Entry>> {
    let mut app = App::load(options)?;
    let mut tui = Tui::new()?;
    let refresh_interval = refresh_interval(options.refresh_seconds);
    let mut last_refresh = Instant::now();

    loop {
        app.poll_probe();
        tui.draw(&app)?;

        if event::poll(poll_timeout(refresh_interval, last_refresh))
            .context("poll terminal event")?
            && let Event::Key(key) = event::read().context("read terminal event")?
        {
            // While renaming, all printable keys edit the name buffer.
            if app.rename.is_some() {
                handle_rename_key(&mut app, key);
            } else if app.confirm_kill.is_some() {
                handle_kill_key(&mut app, key);
            } else if should_quit(key) {
                return Ok(None);
            } else if should_submit(key) {
                return Ok(app.selected_entry().cloned());
            } else {
                match key.code {
                    KeyCode::Char('j' | 'J') | KeyCode::Down => app.next_window(),
                    KeyCode::Char('k' | 'K') | KeyCode::Up => app.previous_window(),
                    KeyCode::Char('l' | 'L')
                    | KeyCode::Right
                    | KeyCode::Tab
                    | KeyCode::PageDown => app.next_session(),
                    KeyCode::Char('h' | 'H')
                    | KeyCode::Left
                    | KeyCode::BackTab
                    | KeyCode::PageUp => app.previous_session(),
                    KeyCode::Char('r' | 'R') => {
                        app.refresh();
                        last_refresh = Instant::now();
                    }
                    KeyCode::Char('$') => app.begin_rename_session(),
                    KeyCode::Char(',') => app.begin_rename_window(),
                    KeyCode::Char('x') => app.begin_kill_window(),
                    KeyCode::Char('X') => app.begin_kill_session(),
                    _ => {}
                }
            }
        }

        if refresh_due(refresh_interval, last_refresh) {
            app.auto_refresh();
            last_refresh = Instant::now();
        }
    }
}

/// Handle a key while an inline rename is in progress: Enter commits, Esc (or
/// Ctrl-C) cancels, Backspace deletes, and printable characters extend the name.
fn handle_rename_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Enter => app.commit_rename(),
        KeyCode::Esc => app.cancel_rename(),
        KeyCode::Backspace => app.rename_backspace(),
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => app.cancel_rename(),
        KeyCode::Char(c)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            app.rename_input(c);
        }
        _ => {}
    }
}

/// Handle a key while a kill is awaiting confirmation: only `y` goes through;
/// `n`, `q`, `Esc`, or Ctrl-C back out. Anything else is ignored so a stray
/// keystroke can't destroy a window or session.
fn handle_kill_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Char('y' | 'Y') => app.commit_kill(),
        KeyCode::Char('n' | 'N' | 'q' | 'Q') | KeyCode::Esc => app.cancel_kill(),
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => app.cancel_kill(),
        _ => {}
    }
}

fn refresh_interval(refresh_seconds: u64) -> Option<Duration> {
    if refresh_seconds == 0 {
        None
    } else {
        Some(Duration::from_secs(refresh_seconds))
    }
}

fn poll_timeout(refresh_interval: Option<Duration>, last_refresh: Instant) -> Duration {
    let max_poll = Duration::from_millis(250);
    let Some(interval) = refresh_interval else {
        return max_poll;
    };

    interval
        .saturating_sub(last_refresh.elapsed())
        .min(max_poll)
}

fn refresh_due(refresh_interval: Option<Duration>, last_refresh: Instant) -> bool {
    refresh_interval.is_some_and(|interval| last_refresh.elapsed() >= interval)
}

fn should_quit(key: KeyEvent) -> bool {
    matches!(
        key,
        KeyEvent {
            code: KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('Q'),
            ..
        } | KeyEvent {
            code: KeyCode::Char('c'),
            modifiers: KeyModifiers::CONTROL,
            ..
        }
    )
}

fn should_submit(key: KeyEvent) -> bool {
    matches!(key.code, KeyCode::Enter)
}

fn render(frame: &mut Frame<'_>, app: &App) {
    let area = frame.area();
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(5),
            Constraint::Min(5),
            Constraint::Length(1),
        ])
        .split(area);
    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(36), Constraint::Percentage(64)])
        .split(vertical[1]);

    render_header(frame, vertical[0], app);
    render_list(frame, body[0], app);
    render_preview(frame, body[1], app);
    render_footer(frame, vertical[2], app);
}

fn render_header(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let session_count = app.sessions.len();
    let window_count: usize = app.sessions.iter().map(|s| s.window_indices.len()).sum();
    let running_count = app
        .entries
        .iter()
        .filter(|e| e.activity == Activity::Active)
        .count();
    let summary = Span::styled(
        format!("{session_count} sessions · {window_count} windows · {running_count} running"),
        Style::default().fg(Color::Cyan),
    );
    let current_session = app
        .current_session()
        .map(|s| format!("  → {} ({})", s.name, s.window_indices.len()))
        .unwrap_or_default();

    let title = Line::from(vec![
        Span::styled("rmux-jump", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        summary,
        Span::styled(current_session, Style::default().fg(Color::DarkGray)),
    ]);

    let mut strip: Vec<Span> = vec![Span::styled(
        "Sessions ",
        Style::default().fg(Color::DarkGray),
    )];
    for (idx, session) in app.sessions.iter().enumerate() {
        let is_selected = idx == app.selected_session;
        let is_current = app.current.session_id.as_deref() == Some(session.id.as_str());

        let mut style = Style::default();
        if is_selected {
            style = style
                .bg(Color::Cyan)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD);
        } else if is_current {
            style = style.fg(Color::Green).add_modifier(Modifier::BOLD);
        } else if session.attached {
            style = style.fg(Color::Green);
        }

        let marker = if is_current { "*" } else { "" };
        strip.push(Span::styled(format!(" {}{} ", session.name, marker), style));
        strip.push(Span::raw(" "));
    }
    let sessions_line = Line::from(strip);

    let refresh = if app.refresh_seconds == 0 {
        "auto off".to_string()
    } else {
        format!("auto {}s", app.refresh_seconds)
    };
    let help = Line::from(Span::styled(
        format!(
            "h/l session  j/k window  enter switch  $/, rename ses/win  X/x kill ses/win  r refresh  q/esc quit  ·  {refresh}"
        ),
        Style::default().fg(Color::DarkGray),
    ));

    let paragraph = Paragraph::new(vec![title, sessions_line, help])
        .block(Block::default().borders(Borders::ALL));
    frame.render_widget(paragraph, area);
}

fn render_list(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let title = match app.current_session() {
        Some(session) => format!(
            "{} · {} window{}{}",
            session.name,
            session.window_indices.len(),
            if session.window_indices.len() == 1 {
                ""
            } else {
                "s"
            },
            if session.attached { " · attached" } else { "" },
        ),
        None => "Windows".to_string(),
    };

    let items: Vec<ListItem> = match app.current_session() {
        Some(session) => session
            .window_indices
            .iter()
            .map(|&i| ListItem::new(window_item_lines(&app.entries[i], app)))
            .collect(),
        None => Vec::new(),
    };
    let mut state = ListState::default();
    if !items.is_empty() {
        state.select(Some(app.selected_window));
    }

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(title))
        .highlight_style(
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("> ");
    frame.render_stateful_widget(list, area, &mut state);
}

fn window_item_lines<'a>(entry: &'a Entry, app: &App) -> Vec<Line<'a>> {
    let current = is_current(entry, &app.current);
    let active_output = entry.activity == Activity::Active;

    let mut flags = Vec::new();
    if current {
        flags.push("here");
    }
    if active_output {
        flags.push("running");
    }
    if entry.window_active {
        flags.push("focused");
    }
    let flags = if flags.is_empty() {
        String::new()
    } else {
        format!("  {}", flags.join(","))
    };

    let current_marker = if current {
        Span::styled(
            "●",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Span::raw(" ")
    };
    let activity_marker = if active_output {
        Span::styled(
            "▸",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Span::raw(" ")
    };

    let name_style = if current {
        Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD)
    } else if active_output {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().add_modifier(Modifier::BOLD)
    };

    let mut lines = vec![Line::from(vec![
        current_marker,
        Span::raw(" "),
        activity_marker,
        Span::raw(" "),
        Span::styled(
            format!("{:>3}: ", entry.window_index),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(entry.window_name.clone(), name_style),
        Span::raw(format!("  {}", entry.pane_current_path)),
        Span::styled(flags, Style::default().fg(Color::Yellow)),
    ])];

    for line in tail_non_empty(&entry.preview, app.inline_lines) {
        lines.push(Line::from(Span::styled(
            format!("    {line}"),
            Style::default().fg(Color::DarkGray),
        )));
    }

    lines
}

fn render_preview(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let lines = match app.selected_entry() {
        Some(entry) => {
            let mut lines = vec![
                Line::from(vec![
                    Span::styled(
                        entry.session_name.clone(),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(format!(
                        "  window {}:{}",
                        entry.window_index, entry.window_name
                    )),
                ]),
                Line::from(vec![
                    Span::styled("pane ", Style::default().fg(Color::DarkGray)),
                    Span::raw(entry.pane_id.clone()),
                    Span::styled("  path ", Style::default().fg(Color::DarkGray)),
                    Span::raw(entry.pane_current_path.clone()),
                ]),
                Line::raw(""),
            ];
            lines.extend(entry.preview.iter().map(|line| Line::raw(line.clone())));
            lines
        }
        None => vec![Line::raw("No RMUX targets found.")],
    };

    let paragraph = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title("Preview"))
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

fn render_footer(frame: &mut Frame<'_>, area: Rect, app: &App) {
    if let Some(kill) = &app.confirm_kill {
        let what = match kill.kind {
            KillKind::Session => "session",
            KillKind::Window => "window",
        };
        let line = Line::from(vec![
            Span::styled(
                format!("kill {what} "),
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("\"{}\"?", kill.label),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "   y confirm · n cancel",
                Style::default().fg(Color::DarkGray),
            ),
        ]);
        frame.render_widget(Paragraph::new(line), area);
        return;
    }

    if let Some(rename) = &app.rename {
        let what = match rename.kind {
            RenameKind::Session => "session",
            RenameKind::Window => "window",
        };
        let line = Line::from(vec![
            Span::styled(
                format!("rename {what} "),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("\"{}\" → ", rename.original),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(
                rename.buffer.clone(),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("▏", Style::default().fg(Color::Yellow)),
            Span::styled(
                "   enter save · esc cancel",
                Style::default().fg(Color::DarkGray),
            ),
        ]);
        frame.render_widget(Paragraph::new(line), area);
        return;
    }

    let text = app
        .status
        .as_deref()
        .unwrap_or("Run inside RMUX to switch clients.");
    frame.render_widget(Paragraph::new(text), area);
}

fn is_current(entry: &Entry, current: &CurrentTarget) -> bool {
    current.session_id.as_deref() == Some(entry.session_id.as_str())
        && current.window_id.as_deref() == Some(entry.window_id.as_str())
}

fn load_entries(all_windows: bool, preview_lines: usize) -> Result<Vec<Entry>> {
    run_rmux(load_entries_async(all_windows, preview_lines))
}

#[derive(Debug, Clone)]
struct WindowRuntimeInfo {
    id: String,
    name: String,
    active: bool,
    active_pane_id: Option<PaneId>,
}

async fn load_entries_async(all_windows: bool, preview_lines: usize) -> Result<Vec<Entry>> {
    let rmux = connect_rmux().await?;
    let discovered = rmux
        .find_panes()
        .all()
        .await
        .context("discover RMUX panes")?;
    let windows = load_window_runtime_info(&rmux, &discovered).await?;
    let mut entries = Vec::new();

    for pane in discovered {
        let session_name = pane.session_name.as_str().to_string();
        let key = (session_name.clone(), pane.window_index);
        let Some(window) = windows.get(&key) else {
            continue;
        };
        if window.active_pane_id != Some(pane.pane_id) {
            continue;
        }
        if !all_windows && !window.active {
            continue;
        }

        let info = pane.pane.info().await.with_context(|| {
            format!(
                "read RMUX pane info for {}:{}",
                pane.session_name, pane.window_index
            )
        })?;
        let session_info = info.session(pane.session_id);
        let preview = match capture_preview_from_pane(&pane.pane, preview_lines).await {
            Ok(preview) => preview,
            Err(err) => vec![format!("failed to capture pane: {err:#}")],
        };
        let activity_fingerprint = activity_fingerprint_of(&preview);
        entries.push(Entry {
            session_id: pane.session_id.to_string(),
            session_name,
            session_attached: session_info.is_some_and(|info| info.attached_clients > 0),
            session_activity: session_info.map(|info| info.generation).unwrap_or_default(),
            window_id: window.id.clone(),
            window_index: pane.window_index.to_string(),
            window_index_num: pane.window_index,
            window_name: window.name.clone(),
            window_active: window.active,
            pane_id: pane.pane_id.to_string(),
            pane_id_num: pane.pane_id.as_u32(),
            pane_current_path: pane.working_directory.unwrap_or_else(|| "-".to_string()),
            preview,
            activity: Activity::Unknown,
            activity_fingerprint,
        });
    }

    if entries.is_empty() {
        bail!("no RMUX sessions/windows found");
    }

    Ok(entries)
}

async fn load_window_runtime_info(
    rmux: &Rmux,
    panes: &[rmux_sdk::DiscoveredPane],
) -> Result<HashMap<(String, u32), WindowRuntimeInfo>> {
    let session_names: HashSet<String> = panes
        .iter()
        .map(|pane| pane.session_name.as_str().to_string())
        .collect();
    let mut windows = HashMap::new();

    for name in session_names {
        let session_name = rmux_session_name(&name)?;
        let session = rmux
            .session(session_name.clone())
            .await
            .with_context(|| format!("open RMUX session handle for {name}"))?;
        for window in list_windows_for_session(&session_name)? {
            let index = window.target.window_index();
            let active_pane_id = session
                .window(index)
                .panes()
                .await
                .with_context(|| format!("list RMUX panes for {name}:{index}"))?
                .into_iter()
                .find(|pane| pane.active)
                .map(|pane| pane.id);
            windows.insert(
                (name.clone(), index),
                WindowRuntimeInfo {
                    id: window.window_id,
                    name: window.name.unwrap_or_else(|| "window".to_string()),
                    active: window.active,
                    active_pane_id,
                },
            );
        }
    }

    Ok(windows)
}

fn current_target() -> CurrentTarget {
    if env::var_os("RMUX").is_none() {
        return CurrentTarget::default();
    };

    match display_message(CURRENT_FORMAT) {
        Ok(output) => {
            let mut parts = output.trim().split(SEP);
            CurrentTarget {
                session_id: parts.next().map(str::to_string),
                window_id: parts.next().map(str::to_string),
            }
        }
        Err(_) => CurrentTarget::default(),
    }
}

async fn capture_preview_from_pane(pane: &rmux_sdk::Pane, lines: usize) -> Result<Vec<String>> {
    let capture = pane
        .screenshot()
        .await
        .context("capture RMUX pane snapshot")?;
    Ok(last_lines(
        capture.text.lines().map(clean_line).collect(),
        cmp::max(lines, 1),
    ))
}

async fn capture_preview_by_id(
    rmux: &Rmux,
    session_name: &str,
    pane_id: u32,
    lines: usize,
) -> Result<Vec<String>> {
    let pane = rmux
        .pane_by_id(rmux_session_name(session_name)?, PaneId::new(pane_id))
        .await
        .with_context(|| format!("open RMUX pane handle for {session_name}:%{pane_id}"))?;
    capture_preview_from_pane(&pane, lines).await
}

fn last_lines(mut lines: Vec<String>, count: usize) -> Vec<String> {
    if lines.len() > count {
        lines.drain(..lines.len() - count);
    }
    lines
}

/// Fingerprint a pane's captured content for activity detection. Uses the full
/// joined preview (trailing whitespace stripped) so a change anywhere in the
/// visible region — not just at the cursor — counts as activity.
fn activity_fingerprint_of(preview: &[String]) -> String {
    let mut acc = String::new();
    for line in preview {
        if !acc.is_empty() {
            acc.push('\n');
        }
        acc.push_str(line);
    }
    acc.trim_end().to_string()
}

/// Compare each entry's current fingerprint against a baseline (typically the
/// fingerprint from the previous capture). Differing → `Active`, matching →
/// `Idle`, missing baseline → `Unknown`.
fn apply_activity(entries: &mut [Entry], baseline: &HashMap<String, String>) {
    for entry in entries {
        entry.activity = match baseline.get(&entry.pane_id) {
            Some(prev) if *prev != entry.activity_fingerprint => Activity::Active,
            Some(_) => Activity::Idle,
            None => Activity::Unknown,
        };
    }
}

/// Result of a background activity probe — per-pane Active/Idle verdict plus
/// the latest fingerprint, both keyed by `pane_id`.
struct ActivityProbeResult {
    activity: HashMap<String, Activity>,
    fingerprints: HashMap<String, String>,
}

/// Spawn a background thread that re-samples each pane `activity_samples - 1`
/// more times, `activity_interval` apart, and sends the verdict over a channel.
/// Returns `None` if probing is disabled or there are no entries to probe.
///
/// Running off-thread is important: probing for 23 panes × 3 extra samples ×
/// 300ms interval takes ~1s, which used to block the picker from drawing.
fn spawn_activity_probe(
    entries: &[Entry],
    options: &Options,
) -> Option<Receiver<ActivityProbeResult>> {
    if entries.is_empty() || options.activity_samples < 2 {
        return None;
    }

    let panes: Vec<(String, u32, String)> = entries
        .iter()
        .map(|e| (e.session_name.clone(), e.pane_id_num, e.pane_id.clone()))
        .collect();
    let initial_fingerprints: HashMap<String, String> = entries
        .iter()
        .map(|e| (e.pane_id.clone(), e.activity_fingerprint.clone()))
        .collect();
    let preview_lines = options.preview_lines;
    let samples = options.activity_samples;
    let interval = options.activity_interval;

    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut any_change: HashMap<String, bool> =
            panes.iter().map(|(_, _, id)| (id.clone(), false)).collect();
        let mut latest = initial_fingerprints.clone();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .ok();
        let rmux = runtime
            .as_ref()
            .and_then(|runtime| runtime.block_on(connect_rmux()).ok());

        for _ in 1..samples {
            thread::sleep(interval);
            for (session_name, pane_id_num, pane_id) in &panes {
                let preview = match (&runtime, &rmux) {
                    (Some(runtime), Some(rmux)) => runtime
                        .block_on(capture_preview_by_id(
                            rmux,
                            session_name,
                            *pane_id_num,
                            preview_lines,
                        ))
                        .unwrap_or_default(),
                    _ => Vec::new(),
                };
                let fp = activity_fingerprint_of(&preview);
                if let Some(prev_fp) = latest.get(pane_id) {
                    if *prev_fp != fp {
                        if let Some(flag) = any_change.get_mut(pane_id) {
                            *flag = true;
                        }
                    }
                }
                latest.insert(pane_id.clone(), fp);
            }
        }

        let activity = any_change
            .into_iter()
            .map(|(id, changed)| {
                (
                    id,
                    if changed {
                        Activity::Active
                    } else {
                        Activity::Idle
                    },
                )
            })
            .collect();

        // Receiver may already be dropped if the picker was closed early.
        let _ = tx.send(ActivityProbeResult {
            activity,
            fingerprints: latest,
        });
    });

    Some(rx)
}

fn switch_to(entry: &Entry) -> Result<()> {
    let target = window_target_for_entry(entry)?;
    expect_response(
        rmux_roundtrip(Request::SelectWindow(SelectWindowRequest {
            target: target.clone(),
        }))?,
        "select-window",
    )?;

    let current = current_target();
    if current.session_id.as_deref() == Some(entry.session_id.as_str()) {
        return Ok(());
    }

    if env::var_os("RMUX").is_none() {
        bail!("run inside RMUX to switch clients");
    }

    expect_response(
        rmux_roundtrip(Request::SwitchClientExt3(SwitchClientExt3Request {
            target_client: None,
            target: Some(window_target_string(entry)),
            key_table: None,
            last_session: false,
            next_session: false,
            previous_session: false,
            toggle_read_only: false,
            sort_order: None,
            skip_environment_update: false,
            zoom: false,
        }))?,
        "switch-client",
    )
}

fn rename_session(target: &str, new_name: &str) -> Result<()> {
    expect_response(
        rmux_roundtrip(Request::RenameSession(RenameSessionRequest {
            target: rmux_session_name(target)?,
            new_name: rmux_session_name(new_name)?,
        }))?,
        "rename-session",
    )
}

fn rename_window(target: &str, new_name: &str) -> Result<()> {
    expect_response(
        rmux_roundtrip(Request::RenameWindow(RenameWindowRequest {
            target: parse_window_target(target)?,
            name: new_name.to_string(),
        }))?,
        "rename-window",
    )
}

fn kill_session(target: &str) -> Result<()> {
    expect_response(
        rmux_roundtrip(Request::KillSession(KillSessionRequest {
            target: rmux_session_name(target)?,
            kill_all_except_target: false,
            clear_alerts: false,
        }))?,
        "kill-session",
    )
}

fn kill_window(target: &str) -> Result<()> {
    expect_response(
        rmux_roundtrip(Request::KillWindow(KillWindowRequest {
            target: parse_window_target(target)?,
            kill_all_others: false,
        }))?,
        "kill-window",
    )
}

fn display_message(message: &str) -> Result<String> {
    match rmux_roundtrip(Request::DisplayMessage(DisplayMessageRequest {
        target: None,
        print: true,
        message: Some(message.to_string()),
        empty_target_context: false,
    }))? {
        Response::DisplayMessage(response) => response
            .command_output()
            .map(|output| String::from_utf8_lossy(output.stdout()).to_string())
            .ok_or_else(|| anyhow!("RMUX display-message returned no output")),
        response => unexpected_response("display-message", response),
    }
}

fn list_windows_for_session(session_name: &SessionName) -> Result<Vec<WindowListEntry>> {
    match rmux_roundtrip(Request::ListWindows(ListWindowsRequest {
        target: session_name.clone(),
        format: None,
    }))? {
        Response::ListWindows(response) => Ok(response.windows),
        response => unexpected_response("list-windows", response),
    }
}

fn rmux_roundtrip(request: Request) -> Result<Response> {
    let socket_path = rmux_socket_path()?;
    let mut connection = rmux_client::connect(&socket_path)
        .with_context(|| format!("connect to RMUX daemon at {}", socket_path.display()))?;
    match connection
        .roundtrip(&request)
        .with_context(|| format!("send RMUX {} request", request.command_name()))?
    {
        Response::Error(error) => Err(anyhow!(error.error)),
        response => Ok(response),
    }
}

fn expect_response(response: Response, expected: &'static str) -> Result<()> {
    if response.command_name() == expected {
        Ok(())
    } else {
        unexpected_response(expected, response)
    }
}

fn unexpected_response<T>(expected: &'static str, response: Response) -> Result<T> {
    bail!(
        "RMUX daemon sent `{}` response for `{expected}` request",
        response.command_name()
    )
}

fn rmux_socket_path() -> Result<PathBuf> {
    rmux_client::resolve_socket_path(None, None).context("resolve RMUX socket path")
}

fn rmux_endpoint() -> Result<RmuxEndpoint> {
    #[cfg(unix)]
    {
        Ok(RmuxEndpoint::UnixSocket(rmux_socket_path()?))
    }

    #[cfg(windows)]
    {
        Ok(RmuxEndpoint::WindowsPipe(
            rmux_socket_path()?
                .as_os_str()
                .to_string_lossy()
                .to_string(),
        ))
    }
}

async fn connect_rmux() -> Result<Rmux> {
    Rmux::builder()
        .endpoint(rmux_endpoint()?)
        .connect_or_start()
        .await
        .context("connect to RMUX SDK daemon")
}

fn run_rmux<T>(future: impl Future<Output = Result<T>>) -> Result<T> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("create RMUX SDK runtime")?
        .block_on(future)
}

fn rmux_session_name(value: &str) -> Result<SessionName> {
    SessionName::new(value.to_string())
        .map_err(|err| anyhow!("invalid RMUX session name `{value}`: {err}"))
}

fn parse_window_target(value: &str) -> Result<WindowTarget> {
    match Target::parse(value)
        .map_err(|err| anyhow!("invalid RMUX window target `{value}`: {err}"))?
    {
        Target::Window(target) => Ok(target),
        _ => bail!("RMUX target `{value}` is not a window target"),
    }
}

fn window_target_for_entry(entry: &Entry) -> Result<WindowTarget> {
    Ok(WindowTarget::with_window(
        rmux_session_name(&entry.session_name)?,
        entry.window_index_num,
    ))
}

fn window_target_string(entry: &Entry) -> String {
    format!("{}:{}", entry.session_name, entry.window_index_num)
}

fn tail_non_empty(lines: &[String], count: usize) -> Vec<String> {
    if count == 0 {
        return Vec::new();
    }

    let mut tail = lines
        .iter()
        .rev()
        .filter(|line| !line.trim().is_empty())
        .take(count)
        .cloned()
        .collect::<Vec<_>>();
    tail.reverse();
    tail
}

fn clean_line(line: &str) -> String {
    line.chars()
        .map(|ch| {
            if ch.is_control() && ch != '\t' {
                ' '
            } else {
                ch
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_lines_enables_all_windows_and_sets_line_counts() {
        let cli = Cli::try_parse_from(["rmux-jump", "--window-lines", "10"]).unwrap();
        let options = cli.options();

        assert!(options.all_windows);
        assert_eq!(options.preview_lines, 10);
        assert_eq!(options.inline_lines, 0);
        assert_eq!(options.refresh_seconds, 5);
    }

    #[test]
    fn explicit_window_options_stay_independent_without_preset() {
        let cli = Cli::try_parse_from([
            "rmux-jump",
            "--all-windows",
            "--preview-lines",
            "12",
            "--inline-lines",
            "3",
        ])
        .unwrap();
        let options = cli.options();

        assert!(options.all_windows);
        assert_eq!(options.preview_lines, 12);
        assert_eq!(options.inline_lines, 3);
        assert_eq!(options.refresh_seconds, 5);
    }

    #[test]
    fn refresh_seconds_can_disable_auto_refresh() {
        let cli = Cli::try_parse_from([
            "rmux-jump",
            "--window-lines",
            "10",
            "--refresh-seconds",
            "0",
        ])
        .unwrap();
        let options = cli.options();

        assert_eq!(options.refresh_seconds, 0);
        assert_eq!(refresh_interval(options.refresh_seconds), None);
    }

    #[test]
    fn refresh_due_respects_interval() {
        assert!(refresh_due(
            Some(Duration::from_secs(1)),
            Instant::now() - Duration::from_secs(2)
        ));
        assert!(!refresh_due(Some(Duration::from_secs(5)), Instant::now()));
        assert!(!refresh_due(None, Instant::now() - Duration::from_secs(10)));
    }

    #[test]
    fn window_lines_keeps_left_list_to_one_line_per_target() {
        let cli = Cli::try_parse_from(["rmux-jump", "--window-lines", "10"]).unwrap();
        let options = cli.options();
        let app = test_app_with_entries(&options, vec![test_entry("$0", "0", false)]);
        let entry = app.selected_entry().unwrap();

        assert_eq!(window_item_lines(entry, &app).len(), 1);
    }

    #[test]
    fn tail_non_empty_returns_last_non_empty_lines() {
        let lines = vec![
            "first".to_string(),
            "".to_string(),
            "second".to_string(),
            "  ".to_string(),
            "third".to_string(),
        ];

        assert_eq!(
            tail_non_empty(&lines, 2),
            vec!["second".to_string(), "third".to_string()]
        );
    }

    #[test]
    fn window_navigation_wraps_at_first_and_last_window() {
        let options = default_options();
        let mut app = test_app_with_entries(
            &options,
            vec![test_entry("$0", "0", true), test_entry("$0", "1", false)],
        );

        // Starting at the first window, `k` wraps to the last.
        assert_eq!(app.selected_window, 0);
        app.previous_window();
        assert_eq!(app.selected_window, 1);

        // From the last window, `j` wraps back to the first.
        app.next_window();
        assert_eq!(app.selected_window, 0);
    }

    #[test]
    fn initial_position_lands_on_current_session_and_window() {
        let entries = vec![
            test_entry("$0", "0", false),
            test_entry("$0", "1", true),
            test_entry("$1", "0", false),
            test_entry("$1", "1", true),
        ];
        let sessions = group_sessions(&entries);

        // Pretend the user opened the picker from session $1, window with id of entry[2].
        let current = CurrentTarget {
            session_id: Some("$1".to_string()),
            window_id: Some(entries[2].window_id.clone()),
        };
        let (s, w) = initial_position(&sessions, &entries, &current);
        let landed = &entries[sessions[s].window_indices[w]];
        assert_eq!(landed.session_id, "$1");
        assert_eq!(landed.window_id, entries[2].window_id);
    }

    #[test]
    fn initial_position_falls_back_to_active_window_when_id_missing() {
        let entries = vec![test_entry("$0", "0", false), test_entry("$0", "1", true)];
        let sessions = group_sessions(&entries);
        let current = CurrentTarget {
            session_id: Some("$0".to_string()),
            window_id: Some("@does-not-exist".to_string()),
        };
        let (s, w) = initial_position(&sessions, &entries, &current);
        let landed = &entries[sessions[s].window_indices[w]];
        assert!(landed.window_active);
    }

    #[test]
    fn session_navigation_jumps_to_active_window_of_target_session() {
        let options = default_options();
        let mut app = test_app_with_entries(
            &options,
            vec![
                test_entry("$0", "0", false),
                test_entry("$0", "1", false),
                test_entry("$1", "0", false),
                test_entry("$1", "1", true),
            ],
        );

        assert_eq!(app.selected_session, 0);
        assert_eq!(app.selected_window, 0);

        app.next_session();
        assert_eq!(app.selected_session, 1);
        // session $1's active window is index 1
        assert_eq!(app.selected_window, 1);

        app.previous_session();
        assert_eq!(app.selected_session, 0);
    }

    #[test]
    fn rename_session_starts_from_current_name_and_edits_buffer() {
        let options = default_options();
        let mut app = test_app_with_entries(
            &options,
            vec![test_entry("$0", "0", true), test_entry("$0", "1", false)],
        );

        app.begin_rename_session();
        let rename = app.rename.as_ref().expect("rename started");
        assert_eq!(rename.kind, RenameKind::Session);
        assert_eq!(rename.target_id, "0");
        // session name for "$0" is "0", so buffer seeds with the current name.
        assert_eq!(rename.buffer, "0");

        app.rename_input('x');
        app.rename_input('y');
        assert_eq!(app.rename.as_ref().unwrap().buffer, "0xy");

        app.rename_backspace();
        assert_eq!(app.rename.as_ref().unwrap().buffer, "0x");

        app.cancel_rename();
        assert!(app.rename.is_none());
    }

    #[test]
    fn rename_window_targets_selected_window_id() {
        let options = default_options();
        let mut app = test_app_with_entries(
            &options,
            vec![test_entry("$0", "0", true), test_entry("$0", "1", false)],
        );
        app.selected_window = 1;

        app.begin_rename_window();
        let rename = app.rename.as_ref().expect("rename started");
        assert_eq!(rename.kind, RenameKind::Window);
        assert_eq!(rename.target_id, "0:1");
        assert_eq!(rename.buffer, "window");
    }

    #[test]
    fn kill_window_targets_selected_window_and_cancels() {
        let options = default_options();
        let mut app = test_app_with_entries(
            &options,
            vec![test_entry("$0", "0", true), test_entry("$0", "1", false)],
        );
        app.selected_window = 1;

        app.begin_kill_window();
        let kill = app.confirm_kill.as_ref().expect("kill pending");
        assert_eq!(kill.kind, KillKind::Window);
        assert_eq!(kill.target_id, "0:1");
        assert_eq!(kill.label, "1: window");

        app.cancel_kill();
        assert!(app.confirm_kill.is_none());
    }

    #[test]
    fn kill_session_targets_current_session() {
        let options = default_options();
        let mut app = test_app_with_entries(
            &options,
            vec![test_entry("$0", "0", true), test_entry("$1", "0", false)],
        );
        app.selected_session = 1;

        app.begin_kill_session();
        let kill = app.confirm_kill.as_ref().expect("kill pending");
        assert_eq!(kill.kind, KillKind::Session);
        assert_eq!(kill.target_id, "1");
        assert_eq!(kill.label, "1");
    }

    fn default_options() -> Options {
        Cli::try_parse_from(["rmux-jump", "--window-lines", "10"])
            .unwrap()
            .options()
    }

    fn test_app_with_entries(options: &Options, entries: Vec<Entry>) -> App {
        let sessions = group_sessions(&entries);
        App {
            entries,
            sessions,
            selected_session: 0,
            selected_window: 0,
            preview_lines: options.preview_lines,
            inline_lines: options.inline_lines,
            refresh_seconds: options.refresh_seconds,
            current: CurrentTarget::default(),
            status: None,
            probe: None,
            rename: None,
            confirm_kill: None,
        }
    }

    fn test_entry(session_id: &str, window_index: &str, window_active: bool) -> Entry {
        let session_num = session_id.trim_start_matches('$');
        let window_index_num = window_index.parse().unwrap();
        Entry {
            session_id: session_id.to_string(),
            session_name: session_num.to_string(),
            session_attached: true,
            session_activity: 0,
            window_id: format!("@{}-{}", session_num, window_index),
            window_index: window_index.to_string(),
            window_index_num,
            window_name: "window".to_string(),
            window_active,
            pane_id: format!("%{}{}", session_num, window_index),
            pane_id_num: window_index_num,
            pane_current_path: "/tmp".to_string(),
            preview: Vec::new(),
            activity: Activity::Unknown,
            activity_fingerprint: String::new(),
        }
    }

    #[test]
    fn activity_fingerprint_joins_preview_and_strips_trailing_whitespace() {
        let preview = vec![
            "abc".to_string(),
            "def".to_string(),
            "tail content here  ".to_string(),
        ];
        let fp = activity_fingerprint_of(&preview);
        assert_eq!(fp, "abc\ndef\ntail content here");
    }

    #[test]
    fn apply_activity_marks_changed_panes_as_active_and_stable_panes_as_idle() {
        let mut entries = vec![
            test_entry_with_fingerprint("$0", "0", "same"),
            test_entry_with_fingerprint("$0", "1", "new output"),
            test_entry_with_fingerprint("$1", "0", "no baseline"),
        ];

        let mut baseline = HashMap::new();
        baseline.insert(entries[0].pane_id.clone(), "same".to_string());
        baseline.insert(entries[1].pane_id.clone(), "old output".to_string());

        apply_activity(&mut entries, &baseline);

        assert_eq!(entries[0].activity, Activity::Idle);
        assert_eq!(entries[1].activity, Activity::Active);
        assert_eq!(entries[2].activity, Activity::Unknown);
    }

    fn test_entry_with_fingerprint(session_id: &str, window_index: &str, fp: &str) -> Entry {
        let mut entry = test_entry(session_id, window_index, false);
        entry.activity_fingerprint = fp.to_string();
        entry
    }
}
