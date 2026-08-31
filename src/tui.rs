use std::collections::{BTreeMap, HashMap};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::Duration;

use accio_provider::{
    humanize_until, Account, Fetch, Knob, Outcome, Provider, Severity, Usage, Window,
};
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{prelude::*, widgets::*, DefaultTerminal};

enum UsageState {
    Loading,
    Ready(Usage),
    Error(String),
}

struct FetchMsg {
    provider: usize,
    account: String,
    outcome: Outcome,
}

enum Mode {
    Normal,
    AddName(String),
    AddMethod(Option<String>),
    Configure(ConfigureForm),
    ConfirmDelete(String),
}

// One knob at a time, then free-form KEY=VALUE extras
struct ConfigureForm {
    name: Option<String>,
    knobs: Vec<Knob>,
    step: usize,
    values: BTreeMap<String, String>,
    input: String,
}

enum Action {
    None,
    Quit,
    Add(Option<String>),
}

pub fn run() -> Result<()> {
    let providers = crate::providers()?;
    let mut terminal = ratatui::init();
    let result = App::new(providers).run(&mut terminal);
    ratatui::restore();
    result
}

struct App {
    providers: Vec<Box<dyn Provider>>,
    tab: usize,
    selected: Vec<usize>,
    usage: HashMap<(usize, String), UsageState>,
    mode: Mode,
    status: String,
    tx: Sender<FetchMsg>,
    rx: Receiver<FetchMsg>,
}

impl App {
    fn new(providers: Vec<Box<dyn Provider>>) -> Self {
        let (tx, rx) = channel();
        let selected: Vec<usize> = providers.iter().map(|p| p.active().unwrap_or(0)).collect();
        App {
            providers,
            tab: 0,
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
                    // Leave the TUI entirely while the provider's own login runs.
                    ratatui::restore();
                    let outcome = self.providers[self.tab].add(name.as_deref());
                    *terminal = ratatui::init();
                    match outcome {
                        Ok(msg) => {
                            self.status = msg;
                            self.selected[self.tab] =
                                self.providers[self.tab].active().unwrap_or(0);
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
                    let name = if name.is_empty() { None } else { Some(name) };
                    if self.providers[self.tab].knobs().is_empty() {
                        self.mode = Mode::Normal;
                        return Ok(Action::Add(name));
                    }
                    self.mode = Mode::AddMethod(name);
                }
                KeyCode::Backspace => {
                    input.pop();
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    input.push(c);
                }
                _ => {}
            },
            Mode::AddMethod(name) => match key.code {
                KeyCode::Esc => self.mode = Mode::Normal,
                KeyCode::Char('l') | KeyCode::Enter => {
                    let name = name.clone();
                    self.mode = Mode::Normal;
                    return Ok(Action::Add(name));
                }
                KeyCode::Char('c') => {
                    let name = name.clone();
                    self.mode = Mode::Configure(ConfigureForm {
                        name,
                        knobs: self.providers[self.tab].knobs(),
                        step: 0,
                        values: BTreeMap::new(),
                        input: String::new(),
                    });
                }
                _ => {}
            },
            Mode::Configure(form) => match key.code {
                KeyCode::Esc => {
                    self.mode = Mode::Normal;
                    self.status = "configure cancelled".into();
                }
                KeyCode::Enter => {
                    let input = form.input.trim().to_string();
                    form.input.clear();
                    if form.step < form.knobs.len() {
                        if !input.is_empty() {
                            form.values.insert(form.knobs[form.step].name.clone(), input);
                        }
                        form.step += 1;
                    } else if input.is_empty() {
                        let name = form.name.clone();
                        let values = std::mem::take(&mut form.values);
                        self.mode = Mode::Normal;
                        match self.providers[self.tab].configure(name.as_deref(), &values) {
                            Ok(msg) => {
                                self.status = msg;
                                self.fetch_all();
                            }
                            Err(e) => self.status = format!("configure failed: {e:#}"),
                        }
                    } else if let Some((k, v)) = input.split_once('=') {
                        if !k.trim().is_empty() {
                            form.values.insert(k.trim().to_string(), v.trim().to_string());
                        }
                    }
                }
                KeyCode::Backspace => {
                    form.input.pop();
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    form.input.push(c);
                }
                _ => {}
            },
            Mode::ConfirmDelete(name) => {
                let name = name.clone();
                self.mode = Mode::Normal;
                if matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                    match self.providers[self.tab].delete(&name) {
                        Ok(()) => {
                            self.status = format!("deleted '{name}'");
                            self.usage.remove(&(self.tab, name));
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
        let n = self.providers[self.tab].accounts().len();
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(Action::Quit),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return Ok(Action::Quit)
            }
            KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => {
                self.tab = (self.tab + 1) % self.providers.len();
            }
            KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') => {
                self.tab = (self.tab + self.providers.len() - 1) % self.providers.len();
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if n > 0 {
                    self.selected[self.tab] = (self.selected[self.tab] + 1) % n;
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if n > 0 {
                    self.selected[self.tab] = (self.selected[self.tab] + n - 1) % n;
                }
            }
            KeyCode::Enter => {
                if n == 0 {
                    return Ok(Action::None);
                }
                let sel = self.selected[self.tab];
                let name = self.providers[self.tab]
                    .accounts()
                    .get(sel)
                    .map(|a| a.name.clone())
                    .unwrap_or_default();
                let p = &mut self.providers[self.tab];
                if p.active() == Some(sel) {
                    self.status = format!("'{name}' is already active");
                } else {
                    match p.activate(sel) {
                        Ok(()) => {
                            self.status = format!("switched to '{name}'");
                            self.selected[self.tab] = p.active().unwrap_or(sel);
                        }
                        Err(e) => self.status = format!("switch failed: {e:#}"),
                    }
                }
            }
            KeyCode::Char('a') => self.mode = Mode::AddName(String::new()),
            KeyCode::Char('d') => {
                if n == 0 {
                    return Ok(Action::None);
                }
                let sel = self.selected[self.tab];
                let p = &self.providers[self.tab];
                if p.active() == Some(sel) {
                    self.status = "can't delete the active account - switch away first".into();
                } else {
                    self.mode = Mode::ConfirmDelete(
                        p.accounts().get(sel).map(|a| a.name.clone()).unwrap_or_default(),
                    );
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

    fn clamp_selection(&mut self) {
        for (i, p) in self.providers.iter().enumerate() {
            let n = p.accounts().len();
            let sel = &mut self.selected[i];
            if n == 0 {
                *sel = 0;
            } else if *sel >= n {
                *sel = n - 1;
            }
        }
    }

    fn fetch_all(&mut self) {
        for pi in 0..self.providers.len() {
            if let Err(e) = self.providers[pi].refresh() {
                self.status = format!("{}: {e:#}", self.providers[pi].name());
            }
            for Fetch { account, job } in self.providers[pi].fetches() {
                self.usage.insert((pi, account.clone()), UsageState::Loading);
                let tx = self.tx.clone();
                std::thread::spawn(move || {
                    let _ = tx.send(FetchMsg { provider: pi, account, outcome: job() });
                });
            }
        }
        self.clamp_selection();
    }

    fn drain_fetches(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            if let Some(state) = msg.outcome.state {
                if let Some(p) = self.providers.get_mut(msg.provider) {
                    if let Err(e) = p.absorb_fetch(&msg.account, state) {
                        self.status = format!("failed to save refreshed token: {e:#}");
                    }
                }
            }
            let ustate = match msg.outcome.usage {
                Ok(u) => UsageState::Ready(u),
                Err(e) => UsageState::Error(e),
            };
            self.usage.insert((msg.provider, msg.account), ustate);
        }
    }

    fn usage_state(&self, name: &str) -> Option<&UsageState> {
        self.usage.get(&(self.tab, name.to_string()))
    }

    fn render(&self, f: &mut Frame) {
        let list_height = (self.providers[self.tab].accounts().len() as u16 + 2).max(3);
        let [tabs_area, accounts_area, usage_area, footer_area] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(list_height),
            Constraint::Min(4),
            Constraint::Length(2),
        ])
        .areas(f.area());
        self.render_tabs(f, tabs_area);
        self.render_accounts(f, accounts_area);
        self.render_usage(f, usage_area);
        self.render_footer(f, footer_area);
    }

    fn render_tabs(&self, f: &mut Frame, area: Rect) {
        let mut spans = vec![Span::styled(" accio ", Style::new().bold()), Span::raw(" ")];
        for (i, p) in self.providers.iter().enumerate() {
            if i > 0 {
                spans.push(Span::styled(" │ ", Style::new().dark_gray()));
            }
            spans.push(if i == self.tab {
                Span::styled(p.name().to_string(), Style::new().green().bold())
            } else {
                Span::styled(p.name().to_string(), Style::new().dim())
            });
        }
        f.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn render_accounts(&self, f: &mut Frame, area: Rect) {
        let width = area.width.saturating_sub(6) as usize; // borders, padding, selection arrow
        let p = &self.providers[self.tab];
        let rows = p.accounts();
        let name_w = column(&rows, |r| r.name.chars().count(), 4, 18);
        let mail_w = column(&rows, |r| r.email.as_deref().unwrap_or("-").chars().count(), 5, 30);
        let plan_w = column(&rows, |r| r.plan.as_deref().unwrap_or("").chars().count(), 3, 12);

        let items: Vec<ListItem> = if rows.is_empty() {
            vec![ListItem::new(Line::styled(
                "no accounts yet - press 'a' to add one",
                Style::new().dim(),
            ))]
        } else {
            rows.iter()
                .enumerate()
                .map(|(i, r)| {
                    let active = p.active() == Some(i);
                    let left = vec![
                        Span::styled(
                            pad(&r.name, name_w),
                            if active { Style::new().green().bold() } else { Style::new() },
                        ),
                        Span::raw("  "),
                        Span::styled(
                            pad(r.email.as_deref().unwrap_or("-"), mail_w),
                            Style::new().dim(),
                        ),
                        Span::raw("  "),
                        Span::styled(
                            pad(r.plan.as_deref().unwrap_or(""), plan_w),
                            Style::new().dim(),
                        ),
                        Span::raw("  "),
                        if active {
                            Span::styled("● active", Style::new().green())
                        } else {
                            Span::raw("")
                        },
                    ];
                    ListItem::new(Line::from(justify(left, self.summary(&r.name), width)))
                })
                .collect()
        };

        let mut list_state = ListState::default().with_selected(Some(self.selected[self.tab]));
        f.render_stateful_widget(
            List::new(items)
                .block(panel(format!(" {} accounts ({}) ", p.name(), rows.len())))
                .highlight_symbol("▶ ")
                .highlight_style(Style::new().bold()),
            area,
            &mut list_state,
        );
    }

    // The worst window an account is sitting at
    fn summary(&self, name: &str) -> Vec<Span<'static>> {
        match self.usage_state(name) {
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
        let p = &self.providers[self.tab];
        let rows = p.accounts();
        let (title, lines) = if rows.is_empty() {
            (" usage ".to_string(), info_lines(p.as_ref()))
        } else {
            let name = rows
                .get(self.selected[self.tab])
                .map(|a| a.name.clone())
                .unwrap_or_default();
            let lines = match self.usage_state(&name) {
                None | Some(UsageState::Loading) => {
                    vec![Line::styled("fetching…", Style::new().dim())]
                }
                Some(UsageState::Error(e)) => {
                    vec![Line::styled(format!("unavailable: {e}"), Style::new().red())]
                }
                Some(UsageState::Ready(u)) if u.windows.is_empty() && u.facts.is_empty() => {
                    vec![Line::styled("nothing to show", Style::new().dim())]
                }
                Some(UsageState::Ready(u)) => usage_lines(u, width),
            };
            (format!(" usage - {name} "), lines)
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
            Mode::AddMethod(_) => Line::from(vec![
                Span::styled("add how? ", Style::new().bold()),
                Span::raw("l = login, c = configure, esc = cancel"),
            ]),
            Mode::Configure(form) => {
                let (label, hint) = match form.knobs.get(form.step) {
                    Some(k) => (k.name.clone(), format!("{} - empty skips", k.hint)),
                    None => ("extra env".to_string(), "KEY=VALUE - empty finishes".to_string()),
                };
                let shown = if form.knobs.get(form.step).is_some_and(|k| k.secret) {
                    "*".repeat(form.input.chars().count())
                } else {
                    form.input.clone()
                };
                Line::from(vec![
                    Span::styled(format!("{label} "), Style::new().bold()),
                    Span::styled(format!("({hint}): "), Style::new().dim()),
                    Span::raw(shown),
                    Span::styled("▏", Style::new().dim()),
                ])
            }
            Mode::ConfirmDelete(name) => Line::styled(
                format!("delete '{name}' from accio? (y/N)"),
                Style::new().yellow().bold(),
            ),
            Mode::Normal => Line::styled(self.status.clone(), Style::new().cyan()),
        };
        let help = Line::styled(
            "←/→ provider · ↑/↓ select · enter switch · a add · d delete · r refresh · q quit",
            Style::new().dim(),
        );
        f.render_widget(
            Paragraph::new(vec![top_line, help]).block(Block::new().padding(Padding::horizontal(2))),
            area,
        );
    }
}

// What an empty provider tab manages, straight from the provider itself
fn info_lines(p: &dyn Provider) -> Vec<Line<'static>> {
    let info = p.info();
    if info.is_empty() {
        return Vec::new();
    }
    let label_w = info.iter().map(|(l, _)| l.chars().count()).max().unwrap_or(0).clamp(4, 12);
    let mut lines = vec![Line::raw("")];
    lines.extend(info.iter().map(|(l, v)| {
        Line::from(vec![
            Span::styled(pad(l, label_w), Style::new().dim()),
            Span::raw("  "),
            Span::raw(v.clone()),
        ])
    }));
    lines
}

fn panel(title: String) -> Block<'static> {
    Block::bordered()
        .border_style(Style::new().dark_gray())
        .padding(Padding::horizontal(1))
        .title(title)
}

fn column(rows: &[Account], f: impl Fn(&Account) -> usize, min: usize, max: usize) -> usize {
    rows.iter().map(f).max().unwrap_or(min).clamp(min, max)
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
            format!("  resets in {}", humanize_until(t)),
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
