use std::{
    cmp,
    collections::HashMap,
    env,
    io::{self, Stdout},
    process::{Command, Stdio},
    sync::mpsc::{self, Receiver, TryRecvError},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use clap::Parser;
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    prelude::*,
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
};

const SEP: char = '\u{241F}';
const PANE_FORMAT: &str = "#{session_id}\u{241F}#{session_name}\u{241F}#{session_attached}\u{241F}#{window_id}\u{241F}#{window_index}\u{241F}#{window_name}\u{241F}#{window_active}\u{241F}#{pane_id}\u{241F}#{pane_active}\u{241F}#{pane_current_path}";
const CURRENT_FORMAT: &str = "#{session_id}\u{241F}#{window_id}";

#[derive(Debug, Parser)]
#[command(author, version, about = "Fast tmux session/window picker")]
struct Cli {
    /// Show one row per tmux window instead of one row per session.
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

#[derive(Debug, Clone, Copy)]
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
    window_id: String,
    window_index: String,
    window_name: String,
    window_active: bool,
    pane_id: String,
    pane_current_path: String,
    worktree_info: Option<GitWorktreeInfo>,
    preview: Vec<String>,
    activity: Activity,
    activity_fingerprint: String,
}

#[derive(Debug, Clone)]
struct GitWorktreeInfo {
    kind: GitWorktreeKind,
    top_level: String,
    git_dir: String,
    common_dir: String,
    head: Option<String>,
    branch: Option<String>,
    upstream: Option<String>,
    status_lines: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GitWorktreeKind {
    Main,
    Linked,
}

impl GitWorktreeInfo {
    fn kind_label(&self) -> &'static str {
        match self.kind {
            GitWorktreeKind::Main => "main",
            GitWorktreeKind::Linked => "linked",
        }
    }

    fn ref_label(&self) -> String {
        if let Some(branch) = self.branch.as_deref().filter(|branch| !branch.is_empty()) {
            return branch.to_string();
        }
        self.head
            .as_deref()
            .map(|head| format!("detached {}", short_hash(head)))
            .unwrap_or_else(|| "no HEAD".to_string())
    }

    fn change_count(&self) -> usize {
        self.status_lines
            .iter()
            .filter(|line| !line.starts_with("##"))
            .count()
    }

    fn status_label(&self) -> String {
        match self.change_count() {
            0 => "clean".to_string(),
            1 => "1 change".to_string(),
            n => format!("{n} changes"),
        }
    }

    fn compact_summary(&self) -> String {
        let mut parts = vec![self.ref_label()];
        if self.kind == GitWorktreeKind::Linked {
            parts.push("linked".to_string());
        }
        parts.push(self.status_label());
        format!("git: {}", parts.join(" · "))
    }
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
    /// Indices into `App::entries`, sorted by tmux window index ascending.
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
    fn load(options: Options) -> Result<Self> {
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

    /// Advance to the next window in the flat list, flowing into the next
    /// session at a session boundary and wrapping around at the very end.
    fn next_window(&mut self) {
        let Some(session) = self.current_session() else {
            return;
        };
        if self.selected_window + 1 < session.window_indices.len() {
            self.selected_window += 1;
            return;
        }
        let n = self.sessions.len();
        let mut si = self.selected_session;
        for _ in 0..n {
            si = (si + 1) % n;
            if !self.sessions[si].window_indices.is_empty() {
                self.selected_session = si;
                self.selected_window = 0;
                return;
            }
        }
    }

    /// Step to the previous window in the flat list, flowing into the previous
    /// session's last window at a boundary and wrapping around at the top.
    fn previous_window(&mut self) {
        if self.current_session().is_none() {
            return;
        }
        if self.selected_window > 0 {
            self.selected_window -= 1;
            return;
        }
        let n = self.sessions.len();
        let mut si = self.selected_session;
        for _ in 0..n {
            si = (si + n - 1) % n;
            let len = self.sessions[si].window_indices.len();
            if len > 0 {
                self.selected_session = si;
                self.selected_window = len - 1;
                return;
            }
        }
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
                target_id: session.id.clone(),
                original: session.name.clone(),
                buffer: session.name.clone(),
            });
        }
    }

    fn begin_rename_window(&mut self) {
        if let Some(entry) = self.selected_entry() {
            self.rename = Some(RenameState {
                kind: RenameKind::Window,
                target_id: entry.window_id.clone(),
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
            RenameKind::Session => {
                tmux_status(["rename-session", "-t", rename.target_id.as_str(), new_name])
            }
            RenameKind::Window => {
                tmux_status(["rename-window", "-t", rename.target_id.as_str(), new_name])
            }
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
                target_id: entry.window_id.clone(),
                label: format!("{}: {}", entry.window_index, entry.window_name),
            });
        }
    }

    fn begin_kill_session(&mut self) {
        if let Some(session) = self.current_session() {
            self.confirm_kill = Some(KillState {
                kind: KillKind::Session,
                target_id: session.id.clone(),
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
            KillKind::Session => tmux_status(["kill-session", "-t", kill.target_id.as_str()]),
            KillKind::Window => tmux_status(["kill-window", "-t", kill.target_id.as_str()]),
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
                window_indices: Vec::new(),
            });
        group.window_indices.push(i);
    }

    let mut sessions: Vec<SessionGroup> = map.into_values().collect();
    for session in &mut sessions {
        session.window_indices.sort_by_key(|&i| {
            entries[i]
                .window_index
                .parse::<u32>()
                .unwrap_or(u32::MAX)
        });
    }
    // Stable, predictable order: sort by session name only, so a session keeps
    // its position regardless of attach state or recent activity.
    sessions.sort_by(|a, b| a.name.cmp(&b.name));
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
        execute!(stdout, EnterAlternateScreen).context("enter alternate screen")?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend).context("create terminal backend")?;
        terminal.clear().context("clear terminal")?;
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
        print_targets(options)?;
        return Ok(());
    }

    let target = run_picker(options)?;

    if let Some(entry) = target {
        switch_to(&entry)?;
    }

    Ok(())
}

fn print_targets(options: Options) -> Result<()> {
    let entries = load_entries(options.all_windows, options.preview_lines)?;
    for entry in entries {
        let lines = tail_non_empty(&entry.preview, cmp::max(options.inline_lines, 1));
        let worktree = entry
            .worktree_info
            .as_ref()
            .map(|info| info.compact_summary())
            .unwrap_or_default();
        if options.inline_lines <= 1 {
            let last_line = lines
                .last()
                .cloned()
                .unwrap_or_else(|| "<no output>".to_string());
            println!(
                "{}\t{}:{}\t{}\t{}\t{}",
                entry.session_name,
                entry.window_index,
                entry.window_name,
                entry.pane_current_path,
                worktree,
                last_line
            );
        } else {
            println!(
                "{}\t{}:{}\t{}\t{}",
                entry.session_name,
                entry.window_index,
                entry.window_name,
                entry.pane_current_path,
                worktree
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

fn run_picker(options: Options) -> Result<Option<Entry>> {
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
        KeyCode::Char(c) if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
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
            Constraint::Length(4),
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
        Span::styled("tmux-jump", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        summary,
        Span::styled(current_session, Style::default().fg(Color::DarkGray)),
    ]);

    let refresh = if app.refresh_seconds == 0 {
        "auto off".to_string()
    } else {
        format!("auto {}s", app.refresh_seconds)
    };
    let help = Line::from(Span::styled(
        format!(
            "j/k window  h/l jump session  enter switch  $/, rename ses/win  X/x kill ses/win  r refresh  q/esc quit  ·  {refresh}"
        ),
        Style::default().fg(Color::DarkGray),
    ));

    let paragraph = Paragraph::new(vec![title, help])
        .block(Block::default().borders(Borders::ALL));
    frame.render_widget(paragraph, area);
}

fn render_list(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let session_count = app.sessions.len();
    let window_count: usize = app.sessions.iter().map(|s| s.window_indices.len()).sum();
    let title = format!(
        "{session_count} session{} · {window_count} window{}",
        if session_count == 1 { "" } else { "s" },
        if window_count == 1 { "" } else { "s" },
    );

    // One flat list grouped by session: a header per session, then its windows.
    // `selected_row` is the list index of the currently selected window so the
    // highlight lands on it and the view scrolls to keep it visible.
    let mut items: Vec<ListItem> = Vec::new();
    let mut selected_row = 0usize;
    for (si, session) in app.sessions.iter().enumerate() {
        items.push(session_header_item(session, si == app.selected_session, &app.current));
        for (wi, &ei) in session.window_indices.iter().enumerate() {
            if si == app.selected_session && wi == app.selected_window {
                selected_row = items.len();
            }
            items.push(ListItem::new(window_item_lines(&app.entries[ei], app)));
        }
    }

    let mut state = ListState::default();
    if !items.is_empty() {
        state.select(Some(selected_row));
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

/// A non-selectable group header row for a session in the flat list.
fn session_header_item<'a>(
    session: &'a SessionGroup,
    is_selected: bool,
    current: &CurrentTarget,
) -> ListItem<'a> {
    let is_current = current.session_id.as_deref() == Some(session.id.as_str());
    let count = session.window_indices.len();

    let mut style = Style::default().add_modifier(Modifier::BOLD);
    style = if is_current || session.attached {
        style.fg(Color::Green)
    } else {
        style.fg(Color::Magenta)
    };

    let pointer = if is_selected { "▾ " } else { "  " };
    let marker = if is_current { " *" } else { "" };
    Line::from(vec![
        Span::styled(pointer, Style::default().fg(Color::Cyan)),
        Span::styled(format!("{}{}", session.name, marker), style),
        Span::styled(
            format!("  ({count} window{})", if count == 1 { "" } else { "s" }),
            Style::default().fg(Color::DarkGray),
        ),
    ])
    .into()
}

fn window_item_lines<'a>(entry: &'a Entry, app: &App) -> Vec<Line<'a>> {
    let current = is_current(entry, &app.current);
    let active_output = entry.activity == Activity::Active;

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
    let focused_marker = if entry.window_active {
        Span::styled(
            "◆",
            Style::default()
                .fg(Color::Yellow)
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
        focused_marker,
        Span::raw(" "),
        Span::styled(
            format!("{:>3}: ", entry.window_index),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(entry.window_name.clone(), name_style),
        Span::raw(format!("  {}", entry.pane_current_path)),
    ])];
    if let Some(worktree_info) = &entry.worktree_info {
        lines.push(Line::from(vec![
            Span::raw("      "),
            Span::styled(
                worktree_info.compact_summary(),
                Style::default().fg(Color::Blue),
            ),
        ]));
    }

    for line in tail_non_empty(&entry.preview, app.inline_lines) {
        lines.push(Line::from(Span::styled(
            format!("    {line}"),
            Style::default().fg(Color::DarkGray),
        )));
    }

    lines
}

fn worktree_detail_lines(info: &GitWorktreeInfo) -> Vec<Line<'static>> {
    let mut lines = vec![
        labeled_line(
            "git",
            format!(
                "{} worktree · {} · {}",
                info.kind_label(),
                info.ref_label(),
                info.status_label()
            ),
        ),
        labeled_line("root", info.top_level.clone()),
        labeled_line("git dir", info.git_dir.clone()),
        labeled_line("common dir", info.common_dir.clone()),
    ];
    if let Some(head) = &info.head {
        lines.push(labeled_line("head", head.clone()));
    }
    if let Some(branch) = &info.branch {
        lines.push(labeled_line("branch", branch.clone()));
    }
    if let Some(upstream) = &info.upstream {
        lines.push(labeled_line("upstream", upstream.clone()));
    }
    if info.status_lines.is_empty() {
        lines.push(labeled_line("status", "clean"));
    } else {
        lines.push(Line::from(Span::styled(
            "status",
            Style::default().fg(Color::DarkGray),
        )));
        lines.extend(
            info.status_lines
                .iter()
                .map(|line| Line::from(Span::raw(format!("  {line}")))),
        );
    }
    lines
}

fn labeled_line(label: &'static str, value: impl Into<String>) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label} "), Style::default().fg(Color::DarkGray)),
        Span::raw(value.into()),
    ])
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
            ];
            if let Some(worktree_info) = &entry.worktree_info {
                lines.push(Line::raw(""));
                lines.extend(worktree_detail_lines(worktree_info));
            }
            lines.push(Line::raw(""));
            lines.extend(entry.preview.iter().map(|line| Line::raw(line.clone())));
            lines
        }
        None => vec![Line::raw("No tmux targets found.")],
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
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
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
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("\"{}\" → ", rename.original),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(
                rename.buffer.clone(),
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
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
        .unwrap_or("Run inside tmux for switch-client; outside tmux this will attach.");
    frame.render_widget(Paragraph::new(text), area);
}

fn is_current(entry: &Entry, current: &CurrentTarget) -> bool {
    current.session_id.as_deref() == Some(entry.session_id.as_str())
        && current.window_id.as_deref() == Some(entry.window_id.as_str())
}

fn load_entries(all_windows: bool, preview_lines: usize) -> Result<Vec<Entry>> {
    let output = tmux_output(["list-panes", "-a", "-F", PANE_FORMAT]).context("list tmux panes")?;
    let mut entries = Vec::new();
    let mut worktree_cache: HashMap<String, Option<GitWorktreeInfo>> = HashMap::new();

    for line in output.lines() {
        let Some(row) = PaneRow::parse(line) else {
            continue;
        };
        if !row.pane_active {
            continue;
        }
        if !all_windows && !row.window_active {
            continue;
        }
        let worktree_info = worktree_cache
            .entry(row.pane_current_path.clone())
            .or_insert_with(|| git_worktree_info(&row.pane_current_path))
            .clone();
        let preview = capture_preview(&row.pane_id, preview_lines);
        let activity_fingerprint = activity_fingerprint_of(&preview);
        entries.push(Entry {
            session_id: row.session_id,
            session_name: row.session_name,
            session_attached: row.session_attached,
            window_id: row.window_id,
            window_index: row.window_index,
            window_name: row.window_name,
            window_active: row.window_active,
            pane_id: row.pane_id,
            pane_current_path: row.pane_current_path,
            worktree_info,
            preview,
            activity: Activity::Unknown,
            activity_fingerprint,
        });
    }

    if entries.is_empty() {
        bail!("no tmux sessions/windows found");
    }

    Ok(entries)
}

fn git_worktree_info(path: &str) -> Option<GitWorktreeInfo> {
    if git_output(path, &["rev-parse", "--is-inside-work-tree"]).ok()? != "true" {
        return None;
    }

    let top_level = git_output(path, &["rev-parse", "--show-toplevel"]).ok()?;
    let git_dir = git_output(path, &["rev-parse", "--absolute-git-dir"]).ok()?;
    let common_dir = git_output(
        path,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .or_else(|_| git_output(path, &["rev-parse", "--git-common-dir"]))
    .ok()?;
    let kind = if git_dir == common_dir {
        GitWorktreeKind::Main
    } else {
        GitWorktreeKind::Linked
    };
    let head = git_output(path, &["rev-parse", "--verify", "HEAD"])
        .ok()
        .filter(|head| !head.is_empty());
    let branch = git_output(path, &["branch", "--show-current"])
        .ok()
        .filter(|branch| !branch.is_empty());
    let upstream = git_output(
        path,
        &[
            "rev-parse",
            "--abbrev-ref",
            "--symbolic-full-name",
            "@{upstream}",
        ],
    )
    .ok()
    .filter(|upstream| !upstream.is_empty());
    let status_lines = git_output(path, &["status", "--short", "--branch"])
        .map(|output| output.lines().map(ToString::to_string).collect())
        .unwrap_or_default();

    Some(GitWorktreeInfo {
        kind,
        top_level,
        git_dir,
        common_dir,
        head,
        branch,
        upstream,
        status_lines,
    })
}

fn git_output(path: &str, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("run git -C {path} {}", args.join(" ")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git {} failed: {}", args.join(" "), stderr.trim());
    }

    Ok(String::from_utf8_lossy(&output.stdout)
        .trim_end()
        .to_string())
}

fn short_hash(hash: &str) -> String {
    hash.chars().take(12).collect()
}

#[derive(Debug)]
struct PaneRow {
    session_id: String,
    session_name: String,
    session_attached: bool,
    window_id: String,
    window_index: String,
    window_name: String,
    window_active: bool,
    pane_id: String,
    pane_active: bool,
    pane_current_path: String,
}

impl PaneRow {
    fn parse(line: &str) -> Option<Self> {
        let fields: Vec<&str> = line.split(SEP).collect();
        if fields.len() != 10 {
            return None;
        }
        Some(Self {
            session_id: fields[0].to_string(),
            session_name: fields[1].to_string(),
            session_attached: fields[2] != "0",
            window_id: fields[3].to_string(),
            window_index: fields[4].to_string(),
            window_name: fields[5].to_string(),
            window_active: fields[6] != "0",
            pane_id: fields[7].to_string(),
            pane_active: fields[8] != "0",
            pane_current_path: fields[9].to_string(),
        })
    }
}

fn current_target() -> CurrentTarget {
    if env::var_os("TMUX").is_none() {
        return CurrentTarget::default();
    }

    let Ok(output) = tmux_output(["display-message", "-p", CURRENT_FORMAT]) else {
        return CurrentTarget::default();
    };
    let mut parts = output.trim().split(SEP);
    CurrentTarget {
        session_id: parts.next().map(str::to_string),
        window_id: parts.next().map(str::to_string),
    }
}

fn capture_preview(pane_id: &str, lines: usize) -> Vec<String> {
    let start = format!("-{}", cmp::max(lines, 1));
    match tmux_output(["capture-pane", "-p", "-J", "-t", pane_id, "-S", &start]) {
        Ok(output) => output.lines().map(clean_line).collect(),
        Err(err) => vec![format!("failed to capture pane: {err:#}")],
    }
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

    let pane_ids: Vec<String> = entries.iter().map(|e| e.pane_id.clone()).collect();
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
            pane_ids.iter().map(|id| (id.clone(), false)).collect();
        let mut latest = initial_fingerprints.clone();

        for _ in 1..samples {
            thread::sleep(interval);
            for pane_id in &pane_ids {
                let preview = capture_preview(pane_id, preview_lines);
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
                (id, if changed { Activity::Active } else { Activity::Idle })
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
    tmux_status(["select-window", "-t", entry.window_id.as_str()])
        .with_context(|| format!("select tmux window {}", entry.window_id))?;

    if env::var_os("TMUX").is_some() {
        tmux_status(["switch-client", "-t", entry.session_id.as_str()])
            .with_context(|| format!("switch to tmux session {}", entry.session_name))?;
    } else {
        tmux_status(["attach-session", "-t", entry.session_id.as_str()])
            .with_context(|| format!("attach to tmux session {}", entry.session_name))?;
    }

    Ok(())
}

fn tmux_output<const N: usize>(args: [&str; N]) -> Result<String> {
    let output = Command::new("tmux")
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("run tmux")?;

    if !output.status.success() {
        bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn tmux_status<const N: usize>(args: [&str; N]) -> Result<()> {
    let output = Command::new("tmux")
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .context("run tmux")?;

    if !output.status.success() {
        bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }

    Ok(())
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
        let cli = Cli::try_parse_from(["tmux-jump", "--window-lines", "10"]).unwrap();
        let options = cli.options();

        assert!(options.all_windows);
        assert_eq!(options.preview_lines, 10);
        assert_eq!(options.inline_lines, 0);
        assert_eq!(options.refresh_seconds, 5);
    }

    #[test]
    fn explicit_window_options_stay_independent_without_preset() {
        let cli = Cli::try_parse_from([
            "tmux-jump",
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
            "tmux-jump",
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
        let cli = Cli::try_parse_from(["tmux-jump", "--window-lines", "10"]).unwrap();
        let options = cli.options();
        let app = test_app_with_entries(&options, vec![test_entry("$0", "0", false)]);
        let entry = app.selected_entry().unwrap();

        assert_eq!(window_item_lines(entry, &app).len(), 1);
    }

    #[test]
    fn window_item_uses_front_markers_instead_of_trailing_status_text() {
        let options = default_options();
        let mut entry = test_entry("$0", "0", true);
        entry.pane_current_path =
            "/very/long/path/that/should/not/hide/window/status/markers".to_string();
        entry.activity = Activity::Active;
        let mut app = test_app_with_entries(&options, vec![entry]);
        app.current = CurrentTarget {
            session_id: Some("$0".to_string()),
            window_id: Some(app.entries[0].window_id.clone()),
        };

        let row: String = window_item_lines(&app.entries[0], &app)[0]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();

        assert!(row.starts_with("● ▸ ◆"));
        assert!(!row.contains("here"));
        assert!(!row.contains("running"));
        assert!(!row.contains("focused"));
    }

    #[test]
    fn worktree_summary_omits_main_worktree_label() {
        let mut info = test_worktree_info();
        info.kind = GitWorktreeKind::Main;

        assert_eq!(info.compact_summary(), "git: feature · 2 changes");
    }

    #[test]
    fn worktree_summary_marks_linked_worktrees() {
        let info = test_worktree_info();

        assert_eq!(info.compact_summary(), "git: feature · linked · 2 changes");
    }

    #[test]
    fn window_item_puts_worktree_summary_on_separate_line() {
        let options = default_options();
        let mut entry = test_entry("$0", "0", true);
        entry.worktree_info = Some(test_worktree_info());
        let app = test_app_with_entries(&options, vec![entry]);
        let lines = window_item_lines(&app.entries[0], &app);

        let first_row: String = lines[0]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        let second_row: String = lines[1]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();

        assert!(!first_row.contains("git:"));
        assert_eq!(second_row, "      git: feature · linked · 2 changes");
    }

    #[test]
    fn worktree_detail_lines_show_git_paths_and_status() {
        let info = test_worktree_info();
        let rendered: Vec<String> = worktree_detail_lines(&info)
            .into_iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect();

        assert!(rendered.iter().any(|line| line.contains("linked worktree")));
        assert!(rendered.iter().any(|line| line == "root /repo"));
        assert!(rendered.iter().any(|line| line == "common dir /repo/.git"));
        assert!(
            rendered
                .iter()
                .any(|line| line == "upstream origin/feature")
        );
        assert!(rendered.iter().any(|line| line == "  ?? README.md"));
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
    fn sessions_sort_by_name_ignoring_attach_state() {
        // "1" is the attached session; "0" is detached. Name still decides the
        // order, so a session keeps its position no matter what is attached.
        let mut entries = vec![test_entry("$1", "0", true), test_entry("$0", "0", true)];
        entries[0].session_attached = true;
        entries[1].session_attached = false;

        let sessions = group_sessions(&entries);
        assert_eq!(sessions[0].name, "0");
        assert_eq!(sessions[1].name, "1");
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
    fn window_navigation_flows_across_sessions() {
        let options = default_options();
        // $0 has two windows, $1 has one. Sorted by name, $0 precedes $1.
        let mut app = test_app_with_entries(
            &options,
            vec![
                test_entry("$0", "0", true),
                test_entry("$0", "1", false),
                test_entry("$1", "0", true),
            ],
        );
        app.selected_session = 0;
        app.selected_window = 0;

        app.next_window();
        assert_eq!((app.selected_session, app.selected_window), (0, 1));

        // At the end of $0's windows, `j` flows into $1.
        app.next_window();
        assert_eq!((app.selected_session, app.selected_window), (1, 0));

        // At the very last window, `j` wraps to the very first.
        app.next_window();
        assert_eq!((app.selected_session, app.selected_window), (0, 0));

        // `k` from the very first wraps to the very last window of the last session.
        app.previous_window();
        assert_eq!((app.selected_session, app.selected_window), (1, 0));

        // `k` flows back into the previous session's last window.
        app.previous_window();
        assert_eq!((app.selected_session, app.selected_window), (0, 1));
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
        let entries = vec![
            test_entry("$0", "0", false),
            test_entry("$0", "1", true),
        ];
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
        assert_eq!(rename.target_id, "$0");
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
        // window_id for ("$0","1") is "@0-1"; original name is "window".
        assert_eq!(rename.target_id, "@0-1");
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
        assert_eq!(kill.target_id, "@0-1");
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
        assert_eq!(kill.target_id, "$1");
        assert_eq!(kill.label, "1");
    }

    fn default_options() -> Options {
        Cli::try_parse_from(["tmux-jump", "--window-lines", "10"])
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
        Entry {
            session_id: session_id.to_string(),
            session_name: session_id.trim_start_matches('$').to_string(),
            session_attached: true,
            window_id: format!("@{}-{}", session_id.trim_start_matches('$'), window_index),
            window_index: window_index.to_string(),
            window_name: "window".to_string(),
            window_active,
            pane_id: format!("%{}-{}", session_id.trim_start_matches('$'), window_index),
            pane_current_path: "/tmp".to_string(),
            worktree_info: None,
            preview: Vec::new(),
            activity: Activity::Unknown,
            activity_fingerprint: String::new(),
        }
    }

    fn test_worktree_info() -> GitWorktreeInfo {
        GitWorktreeInfo {
            kind: GitWorktreeKind::Linked,
            top_level: "/repo".to_string(),
            git_dir: "/repo/.git/worktrees/feature".to_string(),
            common_dir: "/repo/.git".to_string(),
            head: Some("1234567890abcdef".to_string()),
            branch: Some("feature".to_string()),
            upstream: Some("origin/feature".to_string()),
            status_lines: vec![
                "## feature...origin/feature".to_string(),
                " M src/main.rs".to_string(),
                "?? README.md".to_string(),
            ],
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
