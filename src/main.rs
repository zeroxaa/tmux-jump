use std::{
    cmp, env,
    io::{self, Stdout},
    process::{Command, Stdio},
    time::{Duration, SystemTime, UNIX_EPOCH},
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

const SEP: char = '\x1f';
const PANE_FORMAT: &str = "#{session_id}\x1f#{session_name}\x1f#{session_attached}\x1f#{session_windows}\x1f#{session_activity}\x1f#{window_id}\x1f#{window_index}\x1f#{window_name}\x1f#{window_active}\x1f#{pane_id}\x1f#{pane_active}\x1f#{pane_current_path}";
const CURRENT_FORMAT: &str = "#{session_id}\x1f#{window_id}";

#[derive(Debug, Parser)]
#[command(author, version, about = "Fast tmux session/window picker")]
struct Cli {
    /// Show one row per tmux window instead of one row per session.
    #[arg(short = 'w', long)]
    all_windows: bool,

    /// Shortcut preset: show all windows with the last N lines embedded per row.
    #[arg(long, value_name = "LINES")]
    window_lines: Option<usize>,

    /// Number of captured lines to show in the preview pane.
    #[arg(short = 'n', long, default_value_t = 10)]
    preview_lines: usize,

    /// Number of preview lines embedded into each row in the left list.
    #[arg(long, default_value_t = 2)]
    inline_lines: usize,

    /// Print targets and exit without opening the picker.
    #[arg(long)]
    list: bool,
}

#[derive(Debug, Clone, Copy)]
struct Options {
    all_windows: bool,
    preview_lines: usize,
    inline_lines: usize,
}

impl Cli {
    fn options(&self) -> Options {
        match self.window_lines {
            Some(lines) => Options {
                all_windows: true,
                preview_lines: lines,
                inline_lines: lines,
            },
            None => Options {
                all_windows: self.all_windows,
                preview_lines: self.preview_lines,
                inline_lines: self.inline_lines,
            },
        }
    }
}

#[derive(Debug, Clone)]
struct Entry {
    session_id: String,
    session_name: String,
    session_attached: bool,
    session_windows: u32,
    session_activity: u64,
    window_id: String,
    window_index: String,
    window_name: String,
    window_active: bool,
    pane_id: String,
    pane_current_path: String,
    preview: Vec<String>,
}

#[derive(Debug, Clone, Default)]
struct CurrentTarget {
    session_id: Option<String>,
    window_id: Option<String>,
}

struct App {
    entries: Vec<Entry>,
    selected: usize,
    preview_lines: usize,
    inline_lines: usize,
    all_windows: bool,
    current: CurrentTarget,
    status: Option<String>,
}

impl App {
    fn load(options: Options) -> Result<Self> {
        let current = current_target();
        let entries = load_entries(options.all_windows, options.preview_lines)?;
        Ok(Self {
            entries,
            selected: 0,
            preview_lines: options.preview_lines,
            inline_lines: options.inline_lines,
            all_windows: options.all_windows,
            current,
            status: None,
        })
    }

    fn refresh(&mut self) {
        match load_entries(self.all_windows, self.preview_lines) {
            Ok(entries) => {
                self.entries = entries;
                self.selected = cmp::min(self.selected, self.entries.len().saturating_sub(1));
                self.current = current_target();
                self.status = Some("refreshed".to_string());
            }
            Err(err) => {
                self.status = Some(format!("refresh failed: {err:#}"));
            }
        }
    }

    fn next(&mut self) {
        if self.entries.is_empty() {
            return;
        }
        self.selected = cmp::min(self.selected + 1, self.entries.len() - 1);
    }

    fn previous(&mut self) {
        if self.entries.is_empty() {
            return;
        }
        self.selected = self.selected.saturating_sub(1);
    }

    fn page_down(&mut self) {
        if self.entries.is_empty() {
            return;
        }
        self.selected = cmp::min(self.selected + 10, self.entries.len() - 1);
    }

    fn page_up(&mut self) {
        self.selected = self.selected.saturating_sub(10);
    }

    fn selected_entry(&self) -> Option<&Entry> {
        self.entries.get(self.selected)
    }
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

    loop {
        tui.draw(&app)?;

        if !event::poll(Duration::from_millis(250)).context("poll terminal event")? {
            continue;
        }

        match event::read().context("read terminal event")? {
            Event::Key(key) if should_quit(key) => return Ok(None),
            Event::Key(key) if should_submit(key) => return Ok(app.selected_entry().cloned()),
            Event::Key(KeyEvent {
                code: KeyCode::Char('j'),
                ..
            })
            | Event::Key(KeyEvent {
                code: KeyCode::Char('J'),
                ..
            })
            | Event::Key(KeyEvent {
                code: KeyCode::Down,
                ..
            }) => app.next(),
            Event::Key(KeyEvent {
                code: KeyCode::Char('k'),
                ..
            })
            | Event::Key(KeyEvent {
                code: KeyCode::Char('K'),
                ..
            })
            | Event::Key(KeyEvent {
                code: KeyCode::Up, ..
            }) => app.previous(),
            Event::Key(KeyEvent {
                code: KeyCode::PageDown,
                ..
            }) => app.page_down(),
            Event::Key(KeyEvent {
                code: KeyCode::PageUp,
                ..
            }) => app.page_up(),
            Event::Key(KeyEvent {
                code: KeyCode::Char('r'),
                ..
            })
            | Event::Key(KeyEvent {
                code: KeyCode::Char('R'),
                ..
            }) => app.refresh(),
            _ => {}
        }
    }
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
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(1),
        ])
        .split(area);
    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(48), Constraint::Percentage(52)])
        .split(vertical[1]);

    render_header(frame, vertical[0], app);
    render_list(frame, body[0], app);
    render_preview(frame, body[1], app);
    render_footer(frame, vertical[2], app);
}

fn render_header(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let mode = if app.all_windows {
        "all windows"
    } else {
        "sessions"
    };
    let title = Line::from(vec![
        Span::styled("tmux-jump", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        Span::styled(mode, Style::default().fg(Color::Cyan)),
        Span::raw(format!("  {} entries", app.entries.len())),
    ]);
    let help = Line::from("j/k move  enter switch  r refresh  q/esc quit");
    let paragraph = Paragraph::new(vec![title, help]).block(Block::default().borders(Borders::ALL));
    frame.render_widget(paragraph, area);
}

fn render_list(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let items: Vec<ListItem> = app
        .entries
        .iter()
        .map(|entry| list_item(entry, app))
        .collect();
    let mut state = ListState::default();
    if !items.is_empty() {
        state.select(Some(app.selected));
    }

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title("Targets"))
        .highlight_style(
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("> ");
    frame.render_stateful_widget(list, area, &mut state);
}

fn list_item<'a>(entry: &'a Entry, app: &App) -> ListItem<'a> {
    let mut flags = Vec::new();
    if is_current(entry, &app.current) {
        flags.push("current");
    } else if entry.session_attached {
        flags.push("attached");
    }
    if app.all_windows && entry.window_active {
        flags.push("active");
    }
    let flags = if flags.is_empty() {
        String::new()
    } else {
        format!(" {}", flags.join(","))
    };

    let mut lines = vec![Line::from(vec![
        Span::styled(
            entry.session_name.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(
            "  {}:{}  {}  {}w  {}{}",
            entry.window_index,
            entry.window_name,
            entry.pane_current_path,
            entry.session_windows,
            format_age(entry.session_activity),
            flags
        )),
    ])];

    for line in tail_non_empty(&entry.preview, app.inline_lines) {
        lines.push(Line::from(Span::styled(
            format!("  {line}"),
            Style::default().fg(Color::DarkGray),
        )));
    }

    ListItem::new(lines)
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
        entries.push(Entry {
            session_id: row.session_id,
            session_name: row.session_name,
            session_attached: row.session_attached,
            session_windows: row.session_windows,
            session_activity: row.session_activity,
            window_id: row.window_id,
            window_index: row.window_index,
            window_name: row.window_name,
            window_active: row.window_active,
            pane_id: row.pane_id,
            pane_current_path: row.pane_current_path,
            preview,
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
    session_windows: u32,
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
        if fields.len() != 12 {
            return None;
        }
        Some(Self {
            session_id: fields[0].to_string(),
            session_name: fields[1].to_string(),
            session_attached: fields[2] != "0",
            session_windows: fields[3].parse().unwrap_or(0),
            session_activity: fields[4].parse().unwrap_or(0),
            window_id: fields[5].to_string(),
            window_index: fields[6].to_string(),
            window_name: fields[7].to_string(),
            window_active: fields[8] != "0",
            pane_id: fields[9].to_string(),
            pane_active: fields[10] != "0",
            pane_current_path: fields[11].to_string(),
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

fn format_age(unix_seconds: u64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(unix_seconds);
    let seconds = now.saturating_sub(unix_seconds);

    if seconds < 60 {
        format!("{seconds}s ago")
    } else if seconds < 60 * 60 {
        format!("{}m ago", seconds / 60)
    } else if seconds < 60 * 60 * 24 {
        format!("{}h ago", seconds / 60 / 60)
    } else {
        format!("{}d ago", seconds / 60 / 60 / 24)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_lines_enables_all_windows_and_sets_line_counts() {
        let cli = Cli::try_parse_from(["tmux-jump", "--window-lines", "5"]).unwrap();
        let options = cli.options();

        assert!(options.all_windows);
        assert_eq!(options.preview_lines, 5);
        assert_eq!(options.inline_lines, 5);
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
    fn navigation_stops_at_first_and_last_entries() {
        let mut app = App {
            entries: vec![test_entry("0"), test_entry("1")],
            selected: 0,
            preview_lines: 5,
            inline_lines: 5,
            all_windows: true,
            current: CurrentTarget::default(),
            status: None,
        };

        app.previous();
        assert_eq!(app.selected, 0);

        app.next();
        app.next();
        assert_eq!(app.selected, 1);
    }

    fn test_entry(window_index: &str) -> Entry {
        Entry {
            session_id: "$0".to_string(),
            session_name: "test".to_string(),
            session_attached: true,
            session_windows: 2,
            session_activity: 0,
            window_id: format!("@{window_index}"),
            window_index: window_index.to_string(),
            window_name: "window".to_string(),
            window_active: false,
            pane_id: "%0".to_string(),
            pane_current_path: "/tmp".to_string(),
            preview: Vec::new(),
        }
    }
}
