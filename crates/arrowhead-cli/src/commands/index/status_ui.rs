//! Ratatui-based UI for the `arrowhead index status` command.

use std::{collections::VecDeque, io, time::Duration};

use anyhow::{Context, Result};
use arrowhead_core::{ActivityState, DaemonStatus, IssueSeverity, StatusFrame};
use arrowhead_daemon::StatusStream;
use chrono::{DateTime, Local, Utc};
use crossterm::{
    cursor,
    event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures::{FutureExt, StreamExt, future};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Gauge, List, ListItem, Paragraph, Wrap},
};
use tokio::{signal, time};

use super::{describe_activity, describe_download_state, describe_issue_severity};

const MAX_EVENTS: usize = 10;
const SPINNER_FRAMES: &[&str] = &["-", "\\", "|", "/"];

/// Render the live status UI, optionally seeded with the latest snapshot.
pub(super) async fn run_status_ui(
    stream: Option<&mut StatusStream>,
    initial_status: Option<DaemonStatus>,
) -> Result<()> {
    let _guard = TerminalGuard::new()?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend).context("failed to initialise terminal backend")?;
    terminal.clear().ok();

    let mut stream_state = StreamState::new(stream);
    let mut app = StatusApp::new(initial_status, stream_state.is_live());
    if !stream_state.is_live() {
        app.set_message(
            "Live stream unavailable – showing latest snapshot (press q to exit)",
            Color::Yellow,
        );
    }

    let mut input = EventStream::new();
    let mut ticker = time::interval(Duration::from_millis(200));
    let mut ctrl_c = signal::ctrl_c().boxed();

    loop {
        terminal
            .draw(|frame| app.render(frame))
            .context("failed to render status UI")?;

        if app.should_exit() {
            break;
        }

        tokio::select! {
            _ = ticker.tick() => {
                app.tick();
            }
            _ = &mut ctrl_c => {
                app.request_exit();
            }
            Some(event) = input.next() => {
                match event {
                    Ok(event) => app.handle_input(event),
                    Err(err) => {
                        app.set_message(format!("input error: {err}"), Color::Red);
                        app.request_exit();
                    }
                }
            }
            frame = stream_state.next_frame() => {
                match frame {
                    Ok(Some(frame)) => app.ingest_frame(frame),
                    Ok(None) => {
                        app.mark_stream_closed();
                    }
                    Err(err) => {
                        app.set_message(format!("stream error: {err}"), Color::Red);
                        app.request_exit();
                    }
                }
            }
        }
    }

    terminal.show_cursor().ok();
    Ok(())
}

struct TerminalGuard;

impl TerminalGuard {
    fn new() -> Result<Self> {
        terminal::enable_raw_mode().context("failed to enable raw mode")?;
        execute!(io::stdout(), EnterAlternateScreen, cursor::Hide)
            .context("failed to configure terminal")?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
        let mut stdout = io::stdout();
        let _ = execute!(stdout, LeaveAlternateScreen, cursor::Show);
    }
}

struct StatusApp {
    status: Option<DaemonStatus>,
    events: VecDeque<StatusEvent>,
    banner: Option<BannerMessage>,
    should_quit: bool,
    spinner_idx: usize,
    stream_active: bool,
}

impl StatusApp {
    fn new(initial_status: Option<DaemonStatus>, stream_active: bool) -> Self {
        let mut app = Self {
            status: initial_status,
            events: VecDeque::new(),
            banner: None,
            should_quit: false,
            spinner_idx: 0,
            stream_active,
        };

        if let Some(status) = &app.status {
            app.push_event(StatusEvent::from_snapshot(status));
        } else {
            app.set_message("Waiting for daemon status…", Color::Yellow);
        }

        app
    }

    fn ingest_frame(&mut self, frame: StatusFrame) {
        self.status = Some(frame.status.clone());
        self.stream_active = true;
        self.banner = None;
        self.push_event(StatusEvent::from_frame(&frame));
    }

    fn mark_stream_closed(&mut self) {
        self.stream_active = false;
        self.set_message("Indexer closed the status stream", Color::Yellow);
        self.push_event(StatusEvent::from_message(
            "Status stream closed",
            Some("No further updates from daemon".to_string()),
        ));
    }

    fn tick(&mut self) {
        self.spinner_idx = (self.spinner_idx + 1) % SPINNER_FRAMES.len();
    }

    fn handle_input(&mut self, event: Event) {
        match event {
            Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                KeyCode::Esc => self.request_exit(),
                KeyCode::Char('q') => self.request_exit(),
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.request_exit()
                }
                _ => {}
            },
            Event::Resize(_, _) => {}
            _ => {}
        }
    }

    fn render(&self, frame: &mut Frame) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Length(3),
                Constraint::Length(9),
                Constraint::Min(6),
                Constraint::Length(3),
            ])
            .split(frame.area());

        self.render_header(frame, chunks[0]);
        self.render_progress(frame, chunks[1]);
        self.render_status(frame, chunks[2]);
        self.render_events(frame, chunks[3]);
        self.render_footer(frame, chunks[4]);
    }

    fn render_header(&self, frame: &mut Frame, area: Rect) {
        let spinner = SPINNER_FRAMES[self.spinner_idx];
        let mut lines = Vec::new();
        let state_label = if self.stream_active {
            Span::styled("live stream active", Style::default().fg(Color::Green))
        } else {
            Span::styled("stream unavailable", Style::default().fg(Color::Yellow))
        };
        lines.push(Line::from(vec![
            Span::styled(
                "arrowhead indexer ",
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw(spinner.to_string()),
            Span::raw("  "),
            state_label,
        ]));
        if let Some(status) = &self.status {
            lines.push(Line::from(vec![
                Span::styled("Updated: ", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(format_timestamp(status.updated_at)),
            ]));
        } else {
            lines.push(Line::from("No status snapshot loaded yet."));
        }
        let block = Block::default()
            .title("Arrowhead status")
            .borders(Borders::ALL);
        frame.render_widget(
            Paragraph::new(lines).wrap(Wrap { trim: true }).block(block),
            area,
        );
    }

    fn render_progress(&self, frame: &mut Frame, area: Rect) {
        let (ratio, label, color) = self.progress_info();
        let gauge = Gauge::default()
            .block(
                Block::default()
                    .title("Indexing progress")
                    .borders(Borders::ALL),
            )
            .gauge_style(Style::default().fg(color))
            .label(label)
            .ratio(ratio);
        frame.render_widget(gauge, area);
    }

    fn render_status(&self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
            .split(area);

        self.render_activity_details(frame, chunks[0]);
        self.render_downloads_and_issues(frame, chunks[1]);
    }

    fn render_activity_details(&self, frame: &mut Frame, area: Rect) {
        let mut lines = Vec::new();
        if let Some(status) = &self.status {
            let activity = describe_activity(status.activity.state).to_string();
            lines.push(Line::from(vec![
                Span::styled("State: ", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(activity),
            ]));
            if let Some(description) = &status.activity.description {
                lines.push(Line::from(vec![
                    Span::styled("Details: ", Style::default().add_modifier(Modifier::BOLD)),
                    Span::raw(description.clone()),
                ]));
            }
            if let Some(note) = &status.activity.note_id {
                lines.push(Line::from(vec![
                    Span::styled("Note: ", Style::default().add_modifier(Modifier::BOLD)),
                    Span::raw(note.clone()),
                ]));
            }
            if status.activity.queued_jobs > 0 {
                lines.push(Line::from(vec![
                    Span::styled(
                        "Queued jobs: ",
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(status.activity.queued_jobs.to_string()),
                ]));
            }
            lines.push(Line::from(vec![
                Span::styled(
                    "Indexed notes: ",
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::raw(status.indexed_notes.to_string()),
            ]));
            lines.push(Line::from(vec![
                Span::styled(
                    "Notes with errors: ",
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::raw(status.error_notes.to_string()),
            ]));
            lines.push(Line::from(vec![
                Span::styled("Log path: ", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(status.log_path.display().to_string()),
            ]));
        } else {
            lines.push(Line::from("Status data unavailable."));
        }

        let block = Block::default()
            .title("Current activity")
            .borders(Borders::ALL);
        frame.render_widget(
            Paragraph::new(lines).wrap(Wrap { trim: true }).block(block),
            area,
        );
    }

    fn render_downloads_and_issues(&self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(area);

        self.render_downloads(frame, chunks[0]);
        self.render_issues(frame, chunks[1]);
    }

    fn render_downloads(&self, frame: &mut Frame, area: Rect) {
        let mut items = Vec::new();
        if let Some(status) = &self.status {
            for download in &status.downloads {
                let mut text = format!(
                    "{} [{}] {}/{}",
                    download.item,
                    describe_download_state(download.state),
                    download.bytes_downloaded,
                    download
                        .bytes_total
                        .map(|total| total.to_string())
                        .unwrap_or_else(|| "?".to_string())
                );
                if let Some(message) = &download.message {
                    text.push_str(&format!(" – {}", message));
                }
                items.push(ListItem::new(text));
            }
        }

        if items.is_empty() {
            items.push(ListItem::new("No downloads in progress."));
        }

        let list =
            List::new(items).block(Block::default().title("Downloads").borders(Borders::ALL));
        frame.render_widget(list, area);
    }

    fn render_issues(&self, frame: &mut Frame, area: Rect) {
        let mut items = Vec::new();
        if let Some(status) = &self.status {
            for issue in &status.issues {
                let severity_style = match issue.severity {
                    IssueSeverity::Info => Style::default().fg(Color::Cyan),
                    IssueSeverity::Warning => Style::default().fg(Color::Yellow),
                    IssueSeverity::Error => Style::default().fg(Color::Red),
                };
                let mut lines = vec![Line::from(vec![
                    Span::styled(
                        format!("[{}]", describe_issue_severity(issue.severity)),
                        severity_style.add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(format!(" {} – {}", issue.code, issue.message)),
                ])];
                if let Some(detail) = &issue.detail {
                    lines.push(Line::from(Span::raw(detail.clone())));
                }
                items.push(ListItem::new(lines));
            }
        }

        if items.is_empty() {
            items.push(ListItem::new("No outstanding issues."));
        }

        let list = List::new(items).block(Block::default().title("Issues").borders(Borders::ALL));
        frame.render_widget(list, area);
    }

    fn render_events(&self, frame: &mut Frame, area: Rect) {
        let items: Vec<ListItem> = if self.events.is_empty() {
            vec![ListItem::new("No events yet.")]
        } else {
            self.events.iter().map(StatusEvent::to_list_item).collect()
        };

        let list = List::new(items).block(
            Block::default()
                .title("Recent activity")
                .borders(Borders::ALL),
        );
        frame.render_widget(list, area);
    }

    fn render_footer(&self, frame: &mut Frame, area: Rect) {
        let mut lines = vec![Line::from(vec![
            Span::styled("Press q", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" to exit • "),
            Span::styled("Ctrl+C", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" to abort stream"),
        ])];
        if let Some(banner) = &self.banner {
            lines.push(Line::from(Span::styled(
                banner.text.clone(),
                Style::default().fg(banner.color),
            )));
        }

        let block = Block::default().title("Controls").borders(Borders::ALL);
        frame.render_widget(Paragraph::new(lines).block(block), area);
    }

    fn push_event(&mut self, event: StatusEvent) {
        self.events.push_front(event);
        while self.events.len() > MAX_EVENTS {
            self.events.pop_back();
        }
    }

    fn progress_info(&self) -> (f64, String, Color) {
        if let Some(status) = &self.status {
            match status.activity.state {
                ActivityState::Faulted => (
                    0.0,
                    "Indexer faulted – check issues".to_string(),
                    Color::Red,
                ),
                ActivityState::Starting => (0.1, "Starting daemon…".to_string(), Color::Yellow),
                ActivityState::Indexing => {
                    let processed = status.indexed_notes as f64;
                    let queued = status.activity.queued_jobs as f64;
                    let total = (processed + queued).max(1.0);
                    let ratio = (processed / total).clamp(0.0, 1.0);
                    let label = if queued > 0.0 {
                        format!(
                            "{:.0}% • {} queued",
                            ratio * 100.0,
                            status.activity.queued_jobs
                        )
                    } else {
                        format!("{:.0}% • {} indexed", ratio * 100.0, status.indexed_notes)
                    };
                    (ratio, label, Color::LightBlue)
                }
                ActivityState::Downloading => (0.5, "Downloading assets…".to_string(), Color::Cyan),
                ActivityState::Removing => {
                    (0.5, "Removing stale entries…".to_string(), Color::Yellow)
                }
                ActivityState::Idle => (
                    1.0,
                    format!("Idle • {} notes indexed", status.indexed_notes),
                    Color::Green,
                ),
            }
        } else {
            (0.0, "Waiting for status…".to_string(), Color::Gray)
        }
    }

    fn set_message(&mut self, message: impl Into<String>, color: Color) {
        self.banner = Some(BannerMessage::new(message, color));
    }

    fn should_exit(&self) -> bool {
        self.should_quit
    }

    fn request_exit(&mut self) {
        self.should_quit = true;
    }
}

struct BannerMessage {
    text: String,
    color: Color,
}

impl BannerMessage {
    fn new(text: impl Into<String>, color: Color) -> Self {
        Self {
            text: text.into(),
            color,
        }
    }
}

struct StatusEvent {
    timestamp: DateTime<Utc>,
    summary: String,
    detail: Option<String>,
}

impl StatusEvent {
    fn from_frame(frame: &StatusFrame) -> Self {
        let (summary, detail) = summarise_status(&frame.status);
        Self {
            timestamp: frame.emitted_at,
            summary,
            detail,
        }
    }

    fn from_snapshot(status: &DaemonStatus) -> Self {
        let (summary, detail) = summarise_status(status);
        Self {
            timestamp: status.updated_at,
            summary,
            detail,
        }
    }

    fn from_message(summary: impl Into<String>, detail: Option<String>) -> Self {
        Self {
            timestamp: Utc::now(),
            summary: summary.into(),
            detail,
        }
    }

    fn to_list_item(&self) -> ListItem {
        let mut lines = vec![Line::from(vec![
            Span::styled(
                format_event_timestamp(self.timestamp),
                Style::default().fg(Color::Blue),
            ),
            Span::raw("  "),
            Span::styled(
                self.summary.clone(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
        ])];
        if let Some(detail) = &self.detail {
            lines.push(Line::from(Span::raw(detail.clone())));
        }

        ListItem::new(lines)
    }
}

struct StreamState<'a> {
    inner: Option<&'a mut StatusStream>,
}

impl<'a> StreamState<'a> {
    fn new(stream: Option<&'a mut StatusStream>) -> Self {
        Self { inner: stream }
    }

    fn is_live(&self) -> bool {
        self.inner.is_some()
    }

    async fn next_frame(&mut self) -> Result<Option<StatusFrame>> {
        match &mut self.inner {
            Some(stream) => stream.next().await,
            None => future::pending::<Result<Option<StatusFrame>>>().await,
        }
    }
}

fn summarise_status(status: &DaemonStatus) -> (String, Option<String>) {
    let base = describe_activity(status.activity.state);
    let summary = if let Some(description) = &status.activity.description {
        format!("{base}: {description}")
    } else if let Some(note) = &status.activity.note_id {
        format!("{base}: {note}")
    } else {
        base.to_string()
    };

    let mut detail = Vec::new();
    detail.push(format!("{} indexed", status.indexed_notes));
    if status.activity.queued_jobs > 0 {
        detail.push(format!("{} queued", status.activity.queued_jobs));
    }
    if status.error_notes > 0 {
        detail.push(format!("{} errors", status.error_notes));
    }

    let detail = if detail.is_empty() {
        None
    } else {
        Some(detail.join(" • "))
    };

    (summary, detail)
}

fn format_timestamp(timestamp: DateTime<Utc>) -> String {
    timestamp
        .with_timezone(&Local)
        .format("%Y-%m-%d %H:%M:%S")
        .to_string()
}

fn format_event_timestamp(timestamp: DateTime<Utc>) -> String {
    timestamp
        .with_timezone(&Local)
        .format("%H:%M:%S")
        .to_string()
}
