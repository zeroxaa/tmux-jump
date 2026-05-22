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
const PANE_FORMAT: &str = "#{session_id}\u{241F}#{session_name}\u{241F}#{session_attached}\u{241F}#{session_activity}\u{241F}#{window_id}\u{241F}#{window_index}\u{241F}#{window_name}\u{241F}#{window_active}\u{241F}#{pane_id}\u{241F}#{pane_active}\u{241F}#{pane_current_path}";
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
    session_activity: u64,
    window_id: String,
    window_index: String,
    window_name: String,
    window_active: bool,
    pane_id: String,
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
    /// Indices into `App::entries`, sorted by tmux window index ascending.
    window_indices: Vec<usize>,
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
        if session.window_indices.is_empty() {
            return;
        }
        self.selected_window = cmp::min(
            self.selected_window + 1,
            session.window_indices.len() - 1,
        );
    }

    fn previous_window(&mut self) {
        if self.current_session().is_none() {
            return;
        }
        self.selected_window = self.selected_window.saturating_sub(1);
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
        session.window_indices.sort_by_key(|&i| {
            entries[i]
                .window_index
                .parse::<u32>()
                .unwrap_or(u32::MAX)
        });
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
        {
            match event::read().context("read terminal event")? {
                Event::Key(key) if should_quit(key) => return Ok(None),
                Event::Key(key) if should_submit(key) => return Ok(app.selected_entry().cloned()),
                Event::Key(KeyEvent {
                    code: KeyCode::Char('j' | 'J') | KeyCode::Down,
                    ..
                }) => app.next_window(),
                Event::Key(KeyEvent {
                    code: KeyCode::Char('k' | 'K') | KeyCode::Up,
                    ..
                }) => app.previous_window(),
                Event::Key(KeyEvent {
                    code:
                        KeyCode::Char('l' | 'L')
                        | KeyCode::Right
                        | KeyCode::Tab
                        | KeyCode::PageDown,
                    ..
                }) => app.next_session(),
                Event::Key(KeyEvent {
                    code:
                        KeyCode::Char('h' | 'H')
                        | KeyCode::Left
                        | KeyCode::BackTab
                        | KeyCode::PageUp,
                    ..
                }) => app.previous_session(),
                Event::Key(KeyEvent {
                    code: KeyCode::Char('r'),
                    ..
                })
                | Event::Key(KeyEvent {
                    code: KeyCode::Char('R'),
                    ..
                }) => {
                    app.refresh();
                    last_refresh = Instant::now();
                }
                _ => {}
            }
        }

        if refresh_due(refresh_interval, last_refresh) {
            app.auto_refresh();
            last_refresh = Instant::now();
        }
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
        Span::styled("tmux-jump", Style::default().add_modifier(Modifier::BOLD)),
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
            style = style
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD);
        } else if session.attached {
            style = style.fg(Color::Green);
        }

        let marker = if is_current { "*" } else { "" };
        strip.push(Span::styled(
            format!(" {}{} ", session.name, marker),
            style,
        ));
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
            "h/l session  j/k window  enter switch  r refresh  q/esc quit  ·  {refresh}"
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
            if session.window_indices.len() == 1 { "" } else { "s" },
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
        None => vec![Line::raw("No tmux targets found.")],
    };

    let paragraph = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title("Preview"))
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

fn render_footer(frame: &mut Frame<'_>, area: Rect, app: &App) {
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
        let preview = capture_preview(&row.pane_id, preview_lines);
        let activity_fingerprint = activity_fingerprint_of(&preview);
        entries.push(Entry {
            session_id: row.session_id,
            session_name: row.session_name,
            session_attached: row.session_attached,
            session_activity: row.session_activity,
            window_id: row.window_id,
            window_index: row.window_index,
            window_name: row.window_name,
            window_active: row.window_active,
            pane_id: row.pane_id,
            pane_current_path: row.pane_current_path,
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

#[derive(Debug)]
struct PaneRow {
    session_id: String,
    session_name: String,
    session_attached: bool,
    session_activity: u64,
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
        if fields.len() != 11 {
            return None;
        }
        Some(Self {
            session_id: fields[0].to_string(),
            session_name: fields[1].to_string(),
            session_attached: fields[2] != "0",
            session_activity: fields[3].parse().unwrap_or(0),
            window_id: fields[4].to_string(),
            window_index: fields[5].to_string(),
            window_name: fields[6].to_string(),
            window_active: fields[7] != "0",
            pane_id: fields[8].to_string(),
            pane_active: fields[9] != "0",
            pane_current_path: fields[10].to_string(),
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
    fn window_navigation_stops_at_first_and_last_window() {
        let options = default_options();
        let mut app = test_app_with_entries(
            &options,
            vec![test_entry("$0", "0", true), test_entry("$0", "1", false)],
        );

        app.previous_window();
        assert_eq!(app.selected_window, 0);

        app.next_window();
        app.next_window();
        assert_eq!(app.selected_window, 1);
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
        }
    }

    fn test_entry(session_id: &str, window_index: &str, window_active: bool) -> Entry {
        Entry {
            session_id: session_id.to_string(),
            session_name: session_id.trim_start_matches('$').to_string(),
            session_attached: true,
            session_activity: 0,
            window_id: format!("@{}-{}", session_id.trim_start_matches('$'), window_index),
            window_index: window_index.to_string(),
            window_name: "window".to_string(),
            window_active,
            pane_id: format!("%{}-{}", session_id.trim_start_matches('$'), window_index),
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
