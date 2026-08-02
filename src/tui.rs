use std::collections::HashMap;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{prelude::*, widgets::*, DefaultTerminal};
use serde_json::Value;

use crate::oauth::{self, Severity, Usage, Window};
use crate::store::{Profile, Store};

enum UsageState {
    Loading,
    Ready(Usage),
    Error(String),
}

struct FetchResult {
    name: String,
    refreshed_creds: Option<Value>,
    usage: std::result::Result<Value, String>,
}

enum Mode {
    Normal,
    AddName(String),
    ConfirmDelete(String),
}

enum Action {
    None,
    Quit,
    Add(Option<String>),
}

pub fn run() -> Result<()> {
    let store = Store::load()?;
    let mut terminal = ratatui::init();
    let result = App::new(store).run(&mut terminal);
    ratatui::restore();
    result
}

struct App {
    store: Store,
    selected: usize,
    usage: HashMap<String, UsageState>,
    mode: Mode,
    status: String,
    tx: Sender<FetchResult>,
    rx: Receiver<FetchResult>,
}

impl App {
    fn new(store: Store) -> Self {
        let (tx, rx) = channel();
        let selected = store.active.unwrap_or(0);
        App {
            store,
            selected,
            usage: HashMap::new(),
            mode: Mode::Normal,
            status: String::new(),
            tx,
            rx,
        }
    }

    fn run(mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        self.fetch_all();
        loop {
            self.drain_fetches();
            terminal.draw(|f| self.render(f))?;
            if !event::poll(Duration::from_millis(150))? {
                continue;
            }
            let ev = event::read()?;
            let key = match ev {
                Event::Key(k) if k.kind == KeyEventKind::Press => k,
                _ => continue,
            };
            match self.on_key(key)? {
                Action::Quit => return Ok(()),
                Action::Add(name) => {
                    // Leave the TUI entirely while Claude Code's own login runs.
                    ratatui::restore();
                    let outcome = self.store.add_account(name.as_deref());
                    *terminal = ratatui::init();
                    match outcome {
                        Ok(msg) => {
                            self.status = msg;
                            self.selected = self.store.active.unwrap_or(0);
                            self.fetch_all();
                        }
                        Err(e) => self.status = format!("add failed: {e:#}"),
                    }
                }
                Action::None => {}
            }
        }
    }

    fn on_key(&mut self, key: KeyEvent) -> Result<Action> {
        match &mut self.mode {
            Mode::Normal => return self.on_key_normal(key),
            Mode::AddName(input) => match key.code {
                KeyCode::Esc => self.mode = Mode::Normal,
                KeyCode::Enter => {
                    let name = input.trim().to_string();
                    self.mode = Mode::Normal;
                    return Ok(Action::Add(if name.is_empty() { None } else { Some(name) }));
                }
                KeyCode::Backspace => {
                    input.pop();
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    input.push(c);
                }
                _ => {}
            },
            Mode::ConfirmDelete(name) => {
                let name = name.clone();
                self.mode = Mode::Normal;
                if matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                    match self.store.delete(&name) {
                        Ok(()) => {
                            self.status = format!("deleted '{name}'");
                            self.usage.remove(&name);
                            self.clamp_selection();
                        }
                        Err(e) => self.status = format!("{e:#}"),
                    }
                } else {
                    self.status = "delete cancelled".into();
                }
            }
        }
        Ok(Action::None)
    }

    fn on_key_normal(&mut self, key: KeyEvent) -> Result<Action> {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(Action::Quit),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return Ok(Action::Quit)
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if !self.store.profiles.is_empty() {
                    self.selected = (self.selected + 1) % self.store.profiles.len();
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if !self.store.profiles.is_empty() {
                    self.selected =
                        (self.selected + self.store.profiles.len() - 1) % self.store.profiles.len();
                }
            }
            KeyCode::Enter => {
                if self.store.profiles.is_empty() {
                    return Ok(Action::None);
                }
                if self.store.active == Some(self.selected) {
                    self.status = format!("'{}' is already active", self.current_name());
                } else {
                    match self.store.activate(self.selected) {
                        Ok(()) => self.status = format!("switched to '{}'", self.current_name()),
                        Err(e) => self.status = format!("switch failed: {e:#}"),
                    }
                }
            }
            KeyCode::Char('a') => self.mode = Mode::AddName(String::new()),
            KeyCode::Char('d') => {
                if self.store.profiles.is_empty() {
                    return Ok(Action::None);
                }
                if self.store.active == Some(self.selected) {
                    self.status = "can't delete the active account - switch away first".into();
                } else {
                    self.mode = Mode::ConfirmDelete(self.current_name());
                }
            }
            KeyCode::Char('r') => {
                self.status.clear();
                self.fetch_all();
            }
            _ => {}
        }
        Ok(Action::None)
    }

    fn current_name(&self) -> String {
        self.store
            .profiles
            .get(self.selected)
            .map(|p| p.name.clone())
            .unwrap_or_default()
    }

    fn clamp_selection(&mut self) {
        if self.store.profiles.is_empty() {
            self.selected = 0;
        } else if self.selected >= self.store.profiles.len() {
            self.selected = self.store.profiles.len() - 1;
        }
    }

    fn fetch_all(&mut self) {
        for i in 0..self.store.profiles.len() {
            let name = self.store.profiles[i].name.clone();
            let creds = self.store.profiles[i].credentials.clone();
            self.usage.insert(name.clone(), UsageState::Loading);
            let tx = self.tx.clone();
            std::thread::spawn(move || {
                let mut creds = creds;
                let refreshed = match oauth::ensure_fresh(&mut creds) {
                    Ok(r) => r,
                    Err(e) => {
                        let _ = tx.send(FetchResult {
                            name,
                            refreshed_creds: None,
                            usage: Err(format!("{e:#}")),
                        });
                        return;
                    }
                };
                let token = creds
                    .pointer("/claudeAiOauth/accessToken")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let usage = oauth::fetch_usage(&token).map_err(|e| format!("{e:#}"));
                let _ = tx.send(FetchResult {
                    name,
                    refreshed_creds: refreshed.then_some(creds),
                    usage,
                });
            });
        }
    }

    fn drain_fetches(&mut self) {
        while let Ok(fr) = self.rx.try_recv() {
            if let Some(creds) = fr.refreshed_creds {
                if let Err(e) = self.store.update_credentials(&fr.name, creds) {
                    self.status = format!("failed to save refreshed token: {e:#}");
                }
            }
            let state = match fr.usage {
                Ok(v) => {
                    let usage = oauth::parse_usage(&v);
                    if usage.windows.is_empty() && usage.facts.is_empty() {
                        UsageState::Error("no usage data in response".into())
                    } else {
                        UsageState::Ready(usage)
                    }
                }
                Err(e) => UsageState::Error(e),
            };
            self.usage.insert(fr.name, state);
        }
    }

    fn render(&self, f: &mut Frame) {
        let list_height = (self.store.profiles.len() as u16 + 2).max(3);
        let [accounts_area, usage_area, footer_area] = Layout::vertical([
            Constraint::Length(list_height),
            Constraint::Min(4),
            Constraint::Length(2),
        ])
        .areas(f.area());
        self.render_accounts(f, accounts_area);
        self.render_usage(f, usage_area);
        self.render_footer(f, footer_area);
    }

    fn render_accounts(&self, f: &mut Frame, area: Rect) {
        let width = area.width.saturating_sub(6) as usize; // borders, padding, selection arrow
        let ps = &self.store.profiles;
        let name_w = column(ps, |p| p.name.chars().count(), 4, 18);
        let mail_w = column(ps, |p| p.email().unwrap_or("-").chars().count(), 5, 30);
        let plan_w = column(ps, |p| p.subscription().unwrap_or("").chars().count(), 3, 12);

        let items: Vec<ListItem> = if self.store.profiles.is_empty() {
            vec![ListItem::new(Line::styled(
                "no accounts yet - press 'a' to add one",
                Style::new().dim(),
            ))]
        } else {
            self.store
                .profiles
                .iter()
                .enumerate()
                .map(|(i, p)| {
                    let active = self.store.active == Some(i);
                    let left = vec![
                        Span::styled(
                            pad(&p.name, name_w),
                            if active { Style::new().green().bold() } else { Style::new() },
                        ),
                        Span::raw("  "),
                        Span::styled(pad(p.email().unwrap_or("-"), mail_w), Style::new().dim()),
                        Span::raw("  "),
                        Span::styled(pad(p.subscription().unwrap_or(""), plan_w), Style::new().dim()),
                        Span::raw("  "),
                        if active {
                            Span::styled("● active", Style::new().green())
                        } else {
                            Span::raw("")
                        },
                    ];
                    ListItem::new(Line::from(justify(left, self.summary(&p.name), width)))
                })
                .collect()
        };

        let mut list_state = ListState::default().with_selected(Some(self.selected));
        f.render_stateful_widget(
            List::new(items)
                .block(panel(format!(
                    " accio - accounts ({}) ",
                    self.store.profiles.len()
                )))
                .highlight_symbol("▶ ")
                .highlight_style(Style::new().bold()),
            area,
            &mut list_state,
        );
    }

    // The worst window an account is sitting at
    fn summary(&self, name: &str) -> Vec<Span<'static>> {
        match self.usage.get(name) {
            None | Some(UsageState::Loading) => {
                vec![Span::styled("fetching…", Style::new().dim())]
            }
            Some(UsageState::Error(_)) => vec![Span::styled("unavailable", Style::new().red())],
            Some(UsageState::Ready(u)) => {
                let peak = u
                    .windows
                    .iter()
                    .max_by(|a, b| a.percent.total_cmp(&b.percent));
                match peak {
                    None => Vec::new(),
                    Some(w) => {
                        let color = severity_color(&w.severity);
                        let mut spans = meter(w.percent, 12, color);
                        spans.push(Span::styled(
                            format!(" {:>5}", percent(w.percent)),
                            Style::new().fg(color),
                        ));
                        spans
                    }
                }
            }
        }
    }

    fn render_usage(&self, f: &mut Frame, area: Rect) {
        let width = area.width.saturating_sub(4) as usize; // borders + padding
        let (title, lines) = match self.store.profiles.get(self.selected) {
            None => (" usage ".to_string(), Vec::new()),
            Some(p) => {
                let lines = match self.usage.get(&p.name) {
                    None | Some(UsageState::Loading) => {
                        vec![Line::styled("fetching…", Style::new().dim())]
                    }
                    Some(UsageState::Error(e)) => {
                        vec![Line::styled(format!("unavailable: {e}"), Style::new().red())]
                    }
                    Some(UsageState::Ready(u)) => usage_lines(u, width),
                };
                (format!(" usage - {} ", p.name), lines)
            }
        };
        f.render_widget(Paragraph::new(lines).block(panel(title)), area);
    }

    fn render_footer(&self, f: &mut Frame, area: Rect) {
        let top_line = match &self.mode {
            Mode::AddName(input) => Line::from(vec![
                Span::styled("name for new account (empty = from email): ", Style::new().bold()),
                Span::raw(input.clone()),
                Span::styled("▏", Style::new().dim()),
            ]),
            Mode::ConfirmDelete(name) => Line::styled(
                format!("delete '{name}' from accio? (y/N)"),
                Style::new().yellow().bold(),
            ),
            Mode::Normal => Line::styled(self.status.clone(), Style::new().cyan()),
        };
        let help = Line::styled(
            "↑/↓ select · enter switch · a add · d delete · r refresh · q quit",
            Style::new().dim(),
        );
        f.render_widget(
            Paragraph::new(vec![top_line, help]).block(Block::new().padding(Padding::horizontal(2))),
            area,
        );
    }
}

fn panel(title: String) -> Block<'static> {
    Block::bordered()
        .border_style(Style::new().dark_gray())
        .padding(Padding::horizontal(1))
        .title(title)
}

fn column(profiles: &[Profile], f: impl Fn(&Profile) -> usize, min: usize, max: usize) -> usize {
    profiles.iter().map(f).max().unwrap_or(min).clamp(min, max)
}

fn usage_lines(u: &Usage, width: usize) -> Vec<Line<'static>> {
    let mut lines = vec![Line::raw("")];
    let label_w = u
        .windows
        .iter()
        .map(|w| w.label.chars().count())
        .max()
        .unwrap_or(0)
        .clamp(6, 28);
    // bar takes whats left
    let bar_w = width.saturating_sub(label_w + 2 + 6 + 2 + 17).clamp(10, 56);
    lines.extend(u.windows.iter().map(|w| usage_line(w, label_w, bar_w)));

    if !u.facts.is_empty() {
        let head = "── details ";
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            format!("{head}{}", "─".repeat(width.saturating_sub(head.chars().count()))),
            Style::new().dark_gray(),
        ));
        let fact_w = u
            .facts
            .iter()
            .map(|f| f.label.chars().count())
            .max()
            .unwrap_or(0)
            .clamp(4, 34);
        lines.extend(u.facts.iter().map(|f| {
            Line::from(vec![
                Span::styled(pad(&f.label, fact_w), Style::new().dim()),
                Span::raw("  "),
                Span::raw(clip(&f.value, width.saturating_sub(fact_w + 2))),
            ])
        }));
    }
    lines
}

fn usage_line(w: &Window, label_w: usize, bar_w: usize) -> Line<'static> {
    let color = severity_color(&w.severity);
    let mut spans = vec![Span::raw(pad(&w.label, label_w)), Span::raw("  ")];
    spans.extend(meter(w.percent, bar_w, color));
    spans.push(Span::styled(
        format!("  {:>6}", percent(w.percent)),
        Style::new().fg(color).bold(),
    ));
    if let Some(t) = w.resets_at {
        spans.push(Span::styled(
            format!("  resets in {}", oauth::humanize_until(t)),
            Style::new().dim(),
        ));
    }
    Line::from(spans)
}

// Bar filled to 8ths of cell
fn meter(pct: f64, width: usize, color: Color) -> Vec<Span<'static>> {
    const PARTIAL: [char; 8] = [' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉'];
    let eighths = (pct.clamp(0.0, 100.0) / 100.0 * (width * 8) as f64).round() as usize;
    let mut bar = "█".repeat(eighths / 8);
    if eighths / 8 < width && eighths % 8 > 0 {
        bar.push(PARTIAL[eighths % 8]);
    }
    let filled = bar.chars().count();
    vec![
        Span::styled(bar, Style::new().fg(color)),
        Span::styled("█".repeat(width - filled), Style::new().dark_gray()),
    ]
}

fn severity_color(s: &Severity) -> Color {
    match s {
        Severity::Normal => Color::Green,
        Severity::Warning => Color::Yellow,
        Severity::Exceeded => Color::Red,
    }
}

fn percent(p: f64) -> String {
    if (p - p.round()).abs() < 0.05 {
        format!("{p:.0}%")
    } else {
        format!("{p:.1}%")
    }
}

// Push the spans against the far edge
fn justify(
    mut left: Vec<Span<'static>>,
    right: Vec<Span<'static>>,
    width: usize,
) -> Vec<Span<'static>> {
    let used: usize = left.iter().chain(right.iter()).map(|s| s.width()).sum();
    left.push(Span::raw(" ".repeat(width.saturating_sub(used).max(2))));
    left.extend(right);
    left
}

fn pad(s: &str, width: usize) -> String {
    let s = clip(s, width);
    format!("{s:<width$}")
}

fn clip(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        return s.to_string();
    }
    s.chars().take(width.saturating_sub(1)).chain(['…']).collect()
}
