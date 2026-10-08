use std::cell::Cell;
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::Duration;

use accio_provider::{
    common_prefix, humanize_until, label, read_json, write_atomic, Account, Fetch, Knob, Metric,
    MetricValue, Outcome, Provider, Scale, Usage,
};
use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{prelude::*, widgets::*, DefaultTerminal};
use serde_json::{json, Map, Value};

const BAR: Color = Color::Cyan;
const SCROLL_STEP: usize = 5;

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
    AddMethod(usize),
    Form(Form),
    ConfirmDelete(String),
    SortPicker(usize),
}

const ADD_METHODS: [&str; 2] = [
    "log in with the provider cli",
    "configure an endpoint or key",
];

// Which metric orders a provider's accounts, unset means the first bounded number seen
#[derive(Clone, Debug, Default, PartialEq)]
struct SortPref {
    metric: Option<Vec<String>>,
    descending: bool,
}

impl SortPref {
    fn arrow(&self) -> &'static str {
        if self.descending {
            "↓"
        } else {
            "↑"
        }
    }
}

#[derive(Default)]
struct Prefs {
    sort: BTreeMap<String, SortPref>,
}

impl Prefs {
    fn load(path: &Path) -> Self {
        let mut prefs = Prefs::default();
        let Some(v) = read_json(path) else {
            return prefs;
        };
        if let Some(sort) = v.get("sort").and_then(Value::as_object) {
            for (provider, s) in sort {
                let metric = s.get("metric").and_then(Value::as_array).map(|segments| {
                    segments
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect::<Vec<_>>()
                });
                prefs.sort.insert(
                    provider.clone(),
                    SortPref {
                        metric: metric.filter(|m| !m.is_empty()),
                        descending: s
                            .get("descending")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    },
                );
            }
        }
        prefs
    }

    fn save(&self, path: &Path) -> Result<()> {
        let sort: Map<String, Value> = self
            .sort
            .iter()
            .map(|(p, s)| {
                (
                    p.clone(),
                    json!({ "metric": s.metric, "descending": s.descending }),
                )
            })
            .collect();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("cant create {}", parent.display()))?;
        }
        write_atomic(
            path,
            serde_json::to_string_pretty(&json!({ "sort": sort }))?.as_bytes(),
        )
    }
}

fn prefs_path() -> Result<PathBuf> {
    Ok(dirs::config_dir()
        .context("cant determine config dir")?
        .join("accio")
        .join("tui.json"))
}

// All fields visible at once, hints live inside the inputs
struct Form {
    title: String,
    fields: Vec<Field>,
    focus: usize,
    action: FormAction,
}

enum FormAction {
    Login,
    Configure { editing: Option<String> },
}

struct Field {
    label: String,
    hint: String,
    value: String,
    secret: bool,
    extra: bool,
}

impl Field {
    fn knob(k: &Knob) -> Self {
        Field {
            label: k.name.clone(),
            hint: format!("{} (optional)", k.hint),
            value: String::new(),
            secret: k.secret,
            extra: false,
        }
    }

    fn extra() -> Self {
        Field {
            label: "extra".to_string(),
            hint: "KEY=VALUE (optional)".to_string(),
            value: String::new(),
            secret: false,
            extra: true,
        }
    }
}

impl Form {
    fn login(provider: &str) -> Self {
        Form {
            title: format!(" add {provider} account "),
            fields: vec![Field {
                label: "name".to_string(),
                hint: "defaults to the account email".to_string(),
                value: String::new(),
                secret: false,
                extra: false,
            }],
            focus: 0,
            action: FormAction::Login,
        }
    }

    fn configure(provider: &str, knobs: &[Knob]) -> Self {
        let mut fields = vec![Field {
            label: "name".to_string(),
            hint: "profile name".to_string(),
            value: String::new(),
            secret: false,
            extra: false,
        }];
        fields.extend(knobs.iter().map(Field::knob));
        fields.push(Field::extra());
        Form {
            title: format!(" configure {provider} profile "),
            fields,
            focus: 0,
            action: FormAction::Configure { editing: None },
        }
    }

    fn edit(name: &str, knobs: &[Knob], mut values: BTreeMap<String, String>) -> Self {
        let mut fields: Vec<Field> = knobs
            .iter()
            .map(|k| {
                let mut f = Field::knob(k);
                f.value = values.remove(&k.name).unwrap_or_default();
                f
            })
            .collect();
        for (k, v) in values {
            let mut f = Field::extra();
            f.value = format!("{k}={v}");
            fields.push(f);
        }
        fields.push(Field::extra());
        Form {
            title: format!(" edit '{name}' "),
            fields,
            focus: 0,
            action: FormAction::Configure {
                editing: Some(name.to_string()),
            },
        }
    }

    // Filled last extra row spawns a fresh one below
    fn grow(&mut self) {
        if self
            .fields
            .last()
            .is_some_and(|f| f.extra && !f.value.trim().is_empty())
        {
            self.fields.push(Field::extra());
        }
    }
}

enum Action {
    None,
    Quit,
    Add(Option<String>),
    Launch(usize),
}

pub fn run() -> Result<()> {
    let providers = crate::providers()?;
    let mut terminal = ratatui::init();
    let result = App::new(providers, prefs_path()?)
        .run(&mut terminal)
        .map(|_| ());
    ratatui::restore();
    result
}

pub fn pick_session(provider: Box<dyn Provider>) -> Result<Option<(Box<dyn Provider>, usize)>> {
    anyhow::ensure!(
        !provider.accounts().is_empty(),
        "no saved {} profiles - run `accio add {}` or `accio configure {}` first",
        provider.name(),
        provider.name(),
        provider.name()
    );
    use std::io::IsTerminal;
    anyhow::ensure!(
        std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
        "the profile picker needs a terminal - use `accio {} --profile NAME` instead",
        provider.name()
    );
    let mut app = App::new(vec![provider], prefs_path()?);
    app.session_picker = true;
    app.status = "Choose a profile for this session".into();
    let mut terminal = ratatui::init();
    let result = app.run(&mut terminal);
    ratatui::restore();
    Ok(result?.map(|idx| (app.providers.remove(0), idx)))
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
    session_picker: bool,
    prefs: Prefs,
    prefs_path: PathBuf,
    usage_scroll: Cell<usize>,
}

impl App {
    fn new(providers: Vec<Box<dyn Provider>>, prefs_path: PathBuf) -> Self {
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
            session_picker: false,
            prefs: Prefs::load(&prefs_path),
            prefs_path,
            usage_scroll: Cell::new(0),
        }
    }

    fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<Option<usize>> {
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
                Action::Quit => return Ok(None),
                Action::Launch(idx) => return Ok(Some(idx)),
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
        if let Mode::SortPicker(cursor) = self.mode {
            self.on_key_picker(cursor, key);
            return Ok(Action::None);
        }
        match &mut self.mode {
            Mode::Normal => return self.on_key_normal(key),
            Mode::SortPicker(_) => {}
            Mode::AddMethod(selected) => match key.code {
                KeyCode::Esc => self.mode = Mode::Normal,
                KeyCode::Up
                | KeyCode::Down
                | KeyCode::Char('j')
                | KeyCode::Char('k')
                | KeyCode::Tab => *selected = 1 - *selected,
                KeyCode::Char('l') => {
                    self.mode = Mode::Form(Form::login(self.providers[self.tab].name()));
                }
                KeyCode::Char('c') => {
                    let p = &self.providers[self.tab];
                    self.mode = Mode::Form(Form::configure(p.name(), &p.knobs()));
                }
                KeyCode::Enter => {
                    let p = &self.providers[self.tab];
                    self.mode = if *selected == 0 {
                        Mode::Form(Form::login(p.name()))
                    } else {
                        Mode::Form(Form::configure(p.name(), &p.knobs()))
                    };
                }
                _ => {}
            },
            Mode::Form(form) => match key.code {
                KeyCode::Esc => {
                    self.mode = Mode::Normal;
                    self.status = "cancelled".into();
                }
                KeyCode::Up | KeyCode::BackTab => form.focus = form.focus.saturating_sub(1),
                KeyCode::Down | KeyCode::Tab => {
                    form.grow();
                    if form.focus + 1 < form.fields.len() {
                        form.focus += 1;
                    }
                }
                KeyCode::Enter => {
                    form.grow();
                    if form.focus + 1 < form.fields.len() {
                        form.focus += 1;
                    } else {
                        return self.submit_form();
                    }
                }
                KeyCode::Backspace => {
                    form.fields[form.focus].value.pop();
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    form.fields[form.focus].value.push(c);
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

    // Enter on the metric already in charge flips the direction instead
    fn on_key_picker(&mut self, cursor: usize, key: KeyEvent) {
        let count = self.metric_union(self.tab).len();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.mode = Mode::Normal,
            KeyCode::Up | KeyCode::Char('k') => {
                self.mode = Mode::SortPicker(cursor.saturating_sub(1));
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.mode = Mode::SortPicker((cursor + 1).min(count.saturating_sub(1)));
            }
            KeyCode::Enter => {
                let picked = self
                    .metric_union(self.tab)
                    .get(cursor)
                    .map(|m| (m.id.clone(), m.path.clone()));
                self.mode = Mode::Normal;
                if let Some((id, path)) = picked {
                    let pref = self.sort_pref(self.tab);
                    let flip = self.sort_id(self.tab).as_deref() == Some(id.as_str());
                    self.set_sort(path, pref.descending != flip);
                }
            }
            _ => {}
        }
    }

    fn submit_form(&mut self) -> Result<Action> {
        let form = match std::mem::replace(&mut self.mode, Mode::Normal) {
            Mode::Form(f) => f,
            other => {
                self.mode = other;
                return Ok(Action::None);
            }
        };
        let field_value = |f: &Field| {
            let v = f.value.trim().to_string();
            (!v.is_empty()).then_some(v)
        };
        match form.action {
            FormAction::Login => Ok(Action::Add(form.fields.first().and_then(field_value))),
            FormAction::Configure { editing } => {
                let mut name = editing;
                let mut values = BTreeMap::new();
                for f in &form.fields {
                    let Some(v) = field_value(f) else { continue };
                    if f.extra {
                        if let Some((k, v)) = v.split_once('=') {
                            if !k.trim().is_empty() {
                                values.insert(k.trim().to_string(), v.trim().to_string());
                            }
                        }
                    } else if f.label == "name" && name.is_none() {
                        name = Some(v);
                    } else if f.label != "name" {
                        values.insert(f.label.clone(), v);
                    }
                }
                match self.providers[self.tab].configure(name.as_deref(), &values) {
                    Ok(msg) => {
                        // Land the selection on the profile the message names
                        if let Some(n) = msg.split('\'').nth(1) {
                            if let Some(i) = self.providers[self.tab]
                                .accounts()
                                .iter()
                                .position(|a| a.name == n)
                            {
                                self.selected[self.tab] = i;
                            }
                        }
                        self.status = msg;
                        self.fetch_all();
                    }
                    Err(e) => self.status = format!("configure failed: {e:#}"),
                }
                Ok(Action::None)
            }
        }
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
                self.usage_scroll.set(0);
            }
            KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') => {
                self.tab = (self.tab + self.providers.len() - 1) % self.providers.len();
                self.usage_scroll.set(0);
            }
            KeyCode::Down | KeyCode::Char('j') => self.step(1),
            KeyCode::Up | KeyCode::Char('k') => self.step(-1),
            KeyCode::PageDown => self.usage_scroll.set(self.usage_scroll.get() + SCROLL_STEP),
            KeyCode::PageUp => self
                .usage_scroll
                .set(self.usage_scroll.get().saturating_sub(SCROLL_STEP)),
            KeyCode::Enter => {
                if n == 0 {
                    return Ok(Action::None);
                }
                let sel = self.selected[self.tab];
                if self.session_picker {
                    return Ok(Action::Launch(sel));
                }
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
            KeyCode::Char('s') => {
                let union = self.metric_union(self.tab);
                if union.is_empty() {
                    self.status = "no metrics to sort by yet".into();
                } else {
                    let current = self.sort_id(self.tab);
                    let cursor = union
                        .iter()
                        .position(|m| Some(m.id.as_str()) == current.as_deref())
                        .unwrap_or(0);
                    self.mode = Mode::SortPicker(cursor);
                }
            }
            KeyCode::Char('S') => match self.sort_path(self.tab) {
                Some(path) => {
                    let descending = self.sort_pref(self.tab).descending;
                    self.set_sort(path, !descending);
                }
                None => self.status = "no metrics to sort by yet".into(),
            },
            KeyCode::Char('a') if !self.session_picker => {
                let p = &self.providers[self.tab];
                self.mode = if p.knobs().is_empty() {
                    Mode::Form(Form::login(p.name()))
                } else {
                    Mode::AddMethod(0)
                };
            }
            KeyCode::Char('e') if !self.session_picker => {
                if n == 0 {
                    return Ok(Action::None);
                }
                let p = &self.providers[self.tab];
                let name = p
                    .accounts()
                    .get(self.selected[self.tab])
                    .map(|a| a.name.clone())
                    .unwrap_or_default();
                let values = p.values(&name);
                if values.is_empty() {
                    self.status = format!(
                        "'{name}' is a login account - only configured profiles are editable"
                    );
                } else {
                    self.mode = Mode::Form(Form::edit(&name, &p.knobs(), values));
                }
            }
            KeyCode::Char('d') if !self.session_picker => {
                if n == 0 {
                    return Ok(Action::None);
                }
                let sel = self.selected[self.tab];
                let p = &self.providers[self.tab];
                if p.active() == Some(sel) {
                    self.status = "can't delete the active account - switch away first".into();
                } else {
                    self.mode = Mode::ConfirmDelete(
                        p.accounts()
                            .get(sel)
                            .map(|a| a.name.clone())
                            .unwrap_or_default(),
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

    // Moves the selection through the sorted list, wrapping at both ends
    fn step(&mut self, delta: isize) {
        let order = self.order(self.tab);
        if order.is_empty() {
            return;
        }
        let pos = order
            .iter()
            .position(|&i| i == self.selected[self.tab])
            .unwrap_or(0) as isize;
        let n = order.len() as isize;
        self.selected[self.tab] = order[((pos + delta).rem_euclid(n)) as usize];
        self.usage_scroll.set(0);
    }

    fn set_sort(&mut self, path: Vec<String>, descending: bool) {
        let provider = self.providers[self.tab].name().to_string();
        let pref = SortPref {
            metric: Some(path),
            descending,
        };
        self.status = format!(
            "{} accounts ordered by {} {}",
            provider,
            label(
                pref.metric.as_deref().unwrap_or_default(),
                self.label_skip(self.tab)
            ),
            pref.arrow()
        );
        self.prefs.sort.insert(provider, pref);
        if let Err(e) = self.prefs.save(&self.prefs_path) {
            self.status = format!("could not save sort preference: {e:#}");
        }
    }

    fn sort_pref(&self, tab: usize) -> SortPref {
        self.prefs
            .sort
            .get(self.providers[tab].name())
            .cloned()
            .unwrap_or_default()
    }

    // Chosen metric path, or the first bounded number any account reported
    fn sort_path(&self, tab: usize) -> Option<Vec<String>> {
        if let Some(path) = self.sort_pref(tab).metric {
            return Some(path);
        }
        let union = self.metric_union(tab);
        union
            .iter()
            .find(|m| {
                matches!(&m.value, MetricValue::Number { scale, .. } if *scale != Scale::Relative)
            })
            .or_else(|| union.iter().find(|m| m.value.is_number()))
            .map(|m| m.path.clone())
    }

    fn sort_id(&self, tab: usize) -> Option<String> {
        self.sort_path(tab).map(|p| p.join("."))
    }

    // Leading path segments shared by every metric on the tab, dropped from labels
    fn label_skip(&self, tab: usize) -> usize {
        common_prefix(self.metric_union(tab).iter().map(|m| m.path.as_slice()))
    }

    // Every metric any account on the tab reported, in the order first seen
    fn metric_union(&self, tab: usize) -> Vec<&Metric> {
        let mut seen: Vec<&Metric> = Vec::new();
        for account in self.providers[tab].accounts() {
            if let Some(UsageState::Ready(u)) = self.usage.get(&(tab, account.name)) {
                for m in &u.metrics {
                    if !seen.iter().any(|s| s.id == m.id) {
                        seen.push(m);
                    }
                }
            }
        }
        seen
    }

    fn metric_of(&self, tab: usize, account: &str, id: &str) -> Option<&Metric> {
        match self.usage.get(&(tab, account.to_string()))? {
            UsageState::Ready(u) => u.metrics.iter().find(|m| m.id == id),
            _ => None,
        }
    }

    // Largest value of one metric across the tab, what relative bars fill against
    fn relative_max(&self, tab: usize, id: &str) -> f64 {
        self.providers[tab]
            .accounts()
            .iter()
            .filter_map(|a| self.metric_of(tab, &a.name, id))
            .filter_map(|m| match m.value {
                MetricValue::Number { value, .. } => Some(value),
                _ => None,
            })
            .fold(0.0, f64::max)
    }

    // Account indexes in display order, accounts without the metric sink to the bottom
    fn order(&self, tab: usize) -> Vec<usize> {
        let accounts = self.providers[tab].accounts();
        let mut idx: Vec<usize> = (0..accounts.len()).collect();
        let Some(id) = self.sort_id(tab) else {
            return idx;
        };
        let descending = self.sort_pref(tab).descending;
        idx.sort_by(|&a, &b| {
            let va = self
                .metric_of(tab, &accounts[a].name, &id)
                .map(|m| &m.value);
            let vb = self
                .metric_of(tab, &accounts[b].name, &id)
                .map(|m| &m.value);
            match (va, vb) {
                (Some(x), Some(y)) if descending => y.compare(x),
                (Some(x), Some(y)) => x.compare(y),
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => Ordering::Equal,
            }
        });
        idx
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
                self.usage
                    .insert((pi, account.clone()), UsageState::Loading);
                let tx = self.tx.clone();
                std::thread::spawn(move || {
                    let _ = tx.send(FetchMsg {
                        provider: pi,
                        account,
                        outcome: job(),
                    });
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
        match &self.mode {
            Mode::AddMethod(selected) => self.render_menu(f, *selected),
            Mode::Form(form) => render_form(f, form),
            Mode::SortPicker(cursor) => self.render_sort_picker(f, *cursor),
            _ => {}
        }
    }

    fn render_menu(&self, f: &mut Frame, selected: usize) {
        let rect = centered(f.area(), 46, 6);
        f.render_widget(Clear, rect);
        let mut lines: Vec<Line> = ADD_METHODS
            .iter()
            .enumerate()
            .map(|(i, m)| {
                let (marker, style) = if i == selected {
                    ("▶ ", Style::new().bold())
                } else {
                    ("  ", Style::new())
                };
                Line::from(vec![
                    Span::raw(marker),
                    Span::styled(format!("{}  ", ['l', 'c'][i]), Style::new().dim()),
                    Span::styled(m.to_string(), style),
                ])
            })
            .collect();
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            "enter select · esc cancel",
            Style::new().dim(),
        ));
        f.render_widget(
            Paragraph::new(lines).block(panel(format!(
                " add {} account ",
                self.providers[self.tab].name()
            ))),
            rect,
        );
    }

    // Each metric the tab knows with the selected account's reading beside it
    fn render_sort_picker(&self, f: &mut Frame, cursor: usize) {
        let tab = self.tab;
        let p = &self.providers[tab];
        let union = self.metric_union(tab);
        let skip = self.label_skip(tab);
        let current = self.sort_id(tab);
        let pref = self.sort_pref(tab);
        let account = p
            .accounts()
            .get(self.selected[tab])
            .map(|a| a.name.clone())
            .unwrap_or_default();
        let labels: Vec<String> = union.iter().map(|m| label(&m.path, skip)).collect();
        let label_w = labels
            .iter()
            .map(|l| l.chars().count())
            .max()
            .unwrap_or(6)
            .clamp(6, (f.area().width as usize / 2).max(6));
        let value_w = 22;
        let items: Vec<ListItem> = union
            .iter()
            .zip(&labels)
            .map(|(m, l)| {
                let marker = if current.as_deref() == Some(m.id.as_str()) {
                    format!("{} ", pref.arrow())
                } else {
                    "  ".to_string()
                };
                let value = self
                    .metric_of(tab, &account, &m.id)
                    .map(|m| m.value.brief())
                    .unwrap_or_else(|| "—".to_string());
                ListItem::new(Line::from(vec![
                    Span::styled(marker, Style::new().fg(BAR).bold()),
                    Span::raw(pad_label(l, label_w)),
                    Span::raw("  "),
                    Span::styled(clip(&value, value_w), Style::new().dim()),
                ]))
            })
            .collect();
        let rect = centered(
            f.area(),
            (label_w + value_w + 12) as u16,
            items.len() as u16 + 5,
        );
        f.render_widget(Clear, rect);
        let block = panel(format!(" sort {} accounts by ", p.name()));
        let inner = block.inner(rect);
        f.render_widget(block, rect);
        let [list_area, hint_area] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(2)]).areas(inner);
        let mut state = ListState::default().with_selected(Some(cursor));
        f.render_stateful_widget(
            List::new(items)
                .highlight_symbol("▶ ")
                .highlight_style(Style::new().bold()),
            list_area,
            &mut state,
        );
        f.render_widget(
            Paragraph::new(vec![
                Line::raw(""),
                Line::styled(
                    "enter choose · enter on the current one flips order · esc cancel",
                    Style::new().dim(),
                ),
            ]),
            hint_area,
        );
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
        let tab = self.tab;
        let p = &self.providers[tab];
        let rows = p.accounts();
        let name_w = column(&rows, |r| r.name.chars().count(), 4, 18);
        let mail_w = column(
            &rows,
            |r| r.email.as_deref().unwrap_or("-").chars().count(),
            5,
            30,
        );
        let plan_w = column(
            &rows,
            |r| r.plan.as_deref().unwrap_or("").chars().count(),
            3,
            12,
        );
        let order = self.order(tab);
        let sort = self.sort_path(tab);
        let sort_id = sort.as_ref().map(|path| path.join("."));
        let relative_max = sort_id
            .as_deref()
            .map_or(0.0, |id| self.relative_max(tab, id));

        let items: Vec<ListItem> = if rows.is_empty() {
            vec![ListItem::new(Line::styled(
                "no accounts yet - press 'a' to add one",
                Style::new().dim(),
            ))]
        } else {
            order
                .iter()
                .map(|&i| {
                    let r = &rows[i];
                    let active = p.active() == Some(i);
                    let left = vec![
                        Span::styled(
                            pad(&r.name, name_w),
                            if active {
                                Style::new().green().bold()
                            } else {
                                Style::new()
                            },
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
                    let right = self.summary(&r.name, sort_id.as_deref(), relative_max);
                    ListItem::new(Line::from(justify(left, right, width)))
                })
                .collect()
        };

        let title = match &sort {
            Some(path) => format!(
                " {} accounts ({}) · {} {} ",
                p.name(),
                rows.len(),
                self.sort_pref(tab).arrow(),
                label(path, self.label_skip(tab))
            ),
            None => format!(" {} accounts ({}) ", p.name(), rows.len()),
        };
        let highlighted = order
            .iter()
            .position(|&i| i == self.selected[tab])
            .unwrap_or(0);
        let mut list_state = ListState::default().with_selected(Some(highlighted));
        f.render_stateful_widget(
            List::new(items)
                .block(panel(title))
                .highlight_symbol("▶ ")
                .highlight_style(Style::new().bold()),
            area,
            &mut list_state,
        );
    }

    // The sort metric's reading for one account row
    fn summary(&self, name: &str, sort_id: Option<&str>, relative_max: f64) -> Vec<Span<'static>> {
        match self.usage_state(name) {
            None | Some(UsageState::Loading) => {
                vec![Span::styled("fetching…", Style::new().dim())]
            }
            Some(UsageState::Error(_)) => vec![Span::styled("unavailable", Style::new().red())],
            Some(UsageState::Ready(_)) => {
                let Some(id) = sort_id else {
                    return Vec::new();
                };
                let Some(m) = self.metric_of(self.tab, name, id) else {
                    return vec![Span::styled("—", Style::new().dim())];
                };
                match m.value.fill(relative_max) {
                    Some(fill) => {
                        let mut spans = meter(fill, 12, bar_style(&m.value));
                        spans.push(Span::styled(
                            format!(" {:>6}", m.value.text()),
                            Style::new().bold(),
                        ));
                        spans
                    }
                    None => vec![Span::styled(clip(&m.value.brief(), 24), Style::new().dim())],
                }
            }
        }
    }

    fn render_usage(&self, f: &mut Frame, area: Rect) {
        let width = area.width.saturating_sub(4) as usize; // borders + padding
        let p = &self.providers[self.tab];
        let rows = p.accounts();
        let (mut title, lines) = if rows.is_empty() {
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
                    vec![Line::styled(
                        format!("unavailable: {e}"),
                        Style::new().red(),
                    )]
                }
                Some(UsageState::Ready(u)) if u.metrics.is_empty() => {
                    vec![Line::styled("nothing to show", Style::new().dim())]
                }
                Some(UsageState::Ready(u)) => self.usage_lines(u, width),
            };
            (format!(" usage - {name} "), lines)
        };
        let visible = area.height.saturating_sub(2) as usize;
        let scroll = self
            .usage_scroll
            .get()
            .min(lines.len().saturating_sub(visible));
        self.usage_scroll.set(scroll);
        if lines.len() > visible {
            title = format!(
                "{title}· {}-{} of {} · pgup/pgdn ",
                scroll + 1,
                (scroll + visible).min(lines.len()),
                lines.len()
            );
        }
        f.render_widget(
            Paragraph::new(lines)
                .block(panel(title))
                .scroll((scroll as u16, 0)),
            area,
        );
    }

    // Numbers as bars first, everything else as details below them
    fn usage_lines(&self, u: &Usage, width: usize) -> Vec<Line<'static>> {
        let tab = self.tab;
        let skip = self.label_skip(tab);
        let sort_id = self.sort_id(tab);
        let arrow = self.sort_pref(tab).arrow();
        let rows: Vec<(String, &Metric)> = u
            .metrics
            .iter()
            .map(|m| (label(&m.path, skip), m))
            .collect();
        let (bars, details): (Vec<_>, Vec<_>) = rows.iter().partition(|(_, m)| m.value.is_number());
        let label_w = rows
            .iter()
            .map(|(l, _)| l.chars().count())
            .max()
            .unwrap_or(6)
            .clamp(6, (width / 2).max(6));
        let value_w = bars
            .iter()
            .map(|(_, m)| m.value.text().chars().count())
            .max()
            .unwrap_or(3)
            .clamp(3, 16);
        let note_w = if bars.iter().any(|(_, m)| m.until.is_some()) {
            20
        } else {
            0
        };
        let bar_w = width
            .saturating_sub(2 + label_w + 2 + 2 + value_w + note_w)
            .clamp(8, 48);

        let marker = |m: &Metric| {
            if sort_id.as_deref() == Some(m.id.as_str()) {
                Span::styled(format!("{arrow} "), Style::new().fg(BAR).bold())
            } else {
                Span::raw("  ")
            }
        };
        let mut lines = vec![Line::raw("")];
        for (l, m) in &bars {
            let fill = m.value.fill(self.relative_max(tab, &m.id)).unwrap_or(0.0);
            let mut spans = vec![marker(m), Span::raw(pad_label(l, label_w)), Span::raw("  ")];
            spans.extend(meter(fill, bar_w, bar_style(&m.value)));
            spans.push(Span::styled(
                format!("  {:>value_w$}", m.value.text()),
                Style::new().bold(),
            ));
            if let Some(until) = &m.until {
                let note = format!("  {} {}", until_verb(&until.key), humanize_until(until.at));
                let used = 2 + label_w + 2 + bar_w + 2 + value_w;
                if used + note.chars().count() <= width {
                    spans.push(Span::styled(note, Style::new().dim()));
                }
            }
            lines.push(Line::from(spans));
        }
        if !details.is_empty() {
            if !bars.is_empty() {
                let head = "── details ";
                lines.push(Line::raw(""));
                lines.push(Line::styled(
                    format!(
                        "{head}{}",
                        "─".repeat(width.saturating_sub(head.chars().count()))
                    ),
                    Style::new().dark_gray(),
                ));
            }
            for (l, m) in &details {
                lines.push(Line::from(vec![
                    marker(m),
                    Span::styled(pad_label(l, label_w), Style::new().dim()),
                    Span::raw("  "),
                    Span::raw(clip(&m.value.text(), width.saturating_sub(2 + label_w + 2))),
                ]));
            }
        }
        lines
    }

    fn render_footer(&self, f: &mut Frame, area: Rect) {
        let top_line = match &self.mode {
            Mode::ConfirmDelete(name) => Line::styled(
                format!("delete '{name}' from accio? (y/N)"),
                Style::new().yellow().bold(),
            ),
            _ => Line::styled(self.status.clone(), Style::new().cyan()),
        };
        let help = Line::styled(
            if self.session_picker {
                "↑/↓ select · enter launch session · s/S sort · r refresh · q/esc cancel"
            } else {
                "←/→ provider · ↑/↓ select · enter switch · s/S sort · a add · e edit · d delete · r refresh · q quit"
            },
            Style::new().dim(),
        );
        f.render_widget(
            Paragraph::new(vec![top_line, help])
                .block(Block::new().padding(Padding::horizontal(2))),
            area,
        );
    }
}

fn render_form(f: &mut Frame, form: &Form) {
    let label_w = form
        .fields
        .iter()
        .map(|fl| fl.label.chars().count())
        .max()
        .unwrap_or(4)
        .clamp(4, 26);
    let rect = centered(f.area(), 66, form.fields.len() as u16 + 4);
    f.render_widget(Clear, rect);
    // borders, padding, marker, label and the gap after it
    let value_w = (rect.width as usize)
        .saturating_sub(4 + 2 + label_w + 2)
        .max(8);

    let mut lines: Vec<Line> = form
        .fields
        .iter()
        .enumerate()
        .map(|(i, fl)| {
            let focused = i == form.focus;
            let marker = if focused { "▶ " } else { "  " };
            let label_style = if focused {
                Style::new().bold()
            } else {
                Style::new().dim()
            };
            let mut spans = vec![
                Span::styled(marker.to_string(), Style::new().cyan()),
                Span::styled(pad(&fl.label, label_w), label_style),
                Span::raw("  "),
            ];
            if fl.value.is_empty() {
                spans.push(Span::styled(
                    clip(&fl.hint, value_w),
                    Style::new().dim().italic(),
                ));
            } else {
                let shown = if fl.secret {
                    "*".repeat(fl.value.chars().count())
                } else {
                    fl.value.clone()
                };
                spans.push(Span::raw(tail(&shown, value_w)));
            }
            if focused {
                spans.push(Span::styled("▏", Style::new().dim()));
            }
            Line::from(spans)
        })
        .collect();
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        "↑/↓ field · enter next, saves on last · esc cancel",
        Style::new().dim(),
    ));
    f.render_widget(Paragraph::new(lines).block(panel(form.title.clone())), rect);
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width.saturating_sub(2));
    let h = height.min(area.height.saturating_sub(2));
    Rect {
        x: area.x + area.width.saturating_sub(w) / 2,
        y: area.y + area.height.saturating_sub(h) / 2,
        width: w,
        height: h,
    }
}

// Keep the end of a long value in view while typing
fn tail(s: &str, width: usize) -> String {
    let n = s.chars().count();
    if n <= width {
        return s.to_string();
    }
    std::iter::once('…')
        .chain(s.chars().skip(n.saturating_sub(width.saturating_sub(1))))
        .collect()
}

// What an empty provider tab manages, straight from the provider itself
fn info_lines(p: &dyn Provider) -> Vec<Line<'static>> {
    let info = p.info();
    if info.is_empty() {
        return Vec::new();
    }
    let label_w = info
        .iter()
        .map(|(l, _)| l.chars().count())
        .max()
        .unwrap_or(0)
        .clamp(4, 12);
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

fn until_verb(key: &str) -> &'static str {
    if key.to_ascii_lowercase().contains("expir") {
        "expires in"
    } else {
        "resets in"
    }
}

// Relative bars read dimmer so a full bar is not mistaken for a hit limit
fn bar_style(v: &MetricValue) -> Style {
    match v {
        MetricValue::Number {
            scale: Scale::Relative,
            ..
        } => Style::new().fg(BAR).dim(),
        _ => Style::new().fg(BAR),
    }
}

// Bar filled to 8ths of cell
fn meter(fill: f64, width: usize, style: Style) -> Vec<Span<'static>> {
    const PARTIAL: [char; 8] = [' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉'];
    let eighths = (fill.clamp(0.0, 1.0) * (width * 8) as f64).round() as usize;
    let mut bar = "█".repeat(eighths / 8);
    if eighths / 8 < width && eighths % 8 > 0 {
        bar.push(PARTIAL[eighths % 8]);
    }
    let filled = bar.chars().count();
    vec![
        Span::styled(bar, style),
        Span::styled("█".repeat(width - filled), Style::new().dark_gray()),
    ]
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

// Metric labels lose their middle so both the group and the leaf stay readable
fn pad_label(s: &str, width: usize) -> String {
    let n = s.chars().count();
    if n <= width {
        return format!("{s:<width$}");
    }
    let head = width.saturating_sub(1) / 2;
    let tail = width.saturating_sub(1) - head;
    let clipped: String = s
        .chars()
        .take(head)
        .chain(['…'])
        .chain(s.chars().skip(n - tail))
        .collect();
    format!("{clipped:<width$}")
}

fn clip(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        return s.to_string();
    }
    s.chars()
        .take(width.saturating_sub(1))
        .chain(['…'])
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use accio_provider::parse_usage;

    struct Fake(Vec<&'static str>);
    impl Provider for Fake {
        fn name(&self) -> &str {
            "fake"
        }
        fn accounts(&self) -> Vec<Account> {
            self.0
                .iter()
                .map(|n| Account {
                    name: n.to_string(),
                    email: None,
                    plan: None,
                })
                .collect()
        }
        fn active(&self) -> Option<usize> {
            Some(0)
        }
        fn activate(&mut self, _: usize) -> Result<()> {
            panic!("picker must not activate")
        }
        fn delete(&mut self, _: &str) -> Result<()> {
            panic!("picker must not delete")
        }
        fn add(&mut self, _: Option<&str>) -> Result<String> {
            panic!("picker must not log in")
        }
        fn fetches(&self) -> Vec<Fetch> {
            Vec::new()
        }
    }

    fn scratch(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("accio-tui-{}-{name}.json", std::process::id()))
    }

    fn press(app: &mut App, code: KeyCode) -> Action {
        app.on_key(KeyEvent::new(code, KeyModifiers::NONE)).unwrap()
    }

    #[test]
    fn session_picker_selects_even_active_profile_and_cancels_without_mutating() {
        let mut app = App::new(vec![Box::new(Fake(vec!["work"]))], scratch("picker"));
        app.session_picker = true;
        for code in [KeyCode::Char('a'), KeyCode::Char('e'), KeyCode::Char('d')] {
            assert!(matches!(press(&mut app, code), Action::None));
            assert!(matches!(app.mode, Mode::Normal));
        }
        assert!(matches!(press(&mut app, KeyCode::Enter), Action::Launch(0)));
        for code in [KeyCode::Esc, KeyCode::Char('q')] {
            assert!(matches!(press(&mut app, code), Action::Quit));
        }
    }

    #[test]
    fn accounts_order_by_the_chosen_metric_and_the_choice_persists() {
        let path = scratch("sort");
        let _ = fs::remove_file(&path);
        let mut app = App::new(vec![Box::new(Fake(vec!["a", "b", "c", "d"]))], path.clone());
        let ready = |v: Value| UsageState::Ready(parse_usage(&v));
        app.usage.insert(
            (0, "a".into()),
            ready(serde_json::json!({"five_hour": {"utilization": 60}, "rows": 10})),
        );
        app.usage.insert(
            (0, "b".into()),
            ready(serde_json::json!({"five_hour": {"utilization": 20}, "rows": 30})),
        );
        app.usage
            .insert((0, "c".into()), ready(serde_json::json!({"rows": 20})));
        app.usage
            .insert((0, "d".into()), UsageState::Error("down".into()));
        assert_eq!(app.sort_id(0).as_deref(), Some("five_hour.utilization"));
        assert_eq!(app.order(0), vec![1, 0, 2, 3]);
        assert_eq!(app.step(1), ());
        assert_eq!(app.selected[0], 2);
        app.step(-2);
        assert_eq!(app.selected[0], 1);

        press(&mut app, KeyCode::Char('s'));
        assert!(matches!(app.mode, Mode::SortPicker(0)));
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.sort_id(0).as_deref(), Some("rows"));
        assert_eq!(app.order(0), vec![0, 2, 1, 3]);
        assert_eq!(app.relative_max(0, "rows"), 30.0);
        press(&mut app, KeyCode::Char('S'));
        assert_eq!(app.order(0), vec![1, 2, 0, 3]);
        assert!(app.status.contains("rows ↓"));
        let mut empty = App::new(vec![Box::new(Fake(vec!["lonely"]))], scratch("empty"));
        press(&mut empty, KeyCode::Char('S'));
        assert_eq!(empty.status, "no metrics to sort by yet");
        assert!(empty.prefs.sort.is_empty());

        let reloaded = Prefs::load(&path);
        assert_eq!(
            reloaded.sort["fake"],
            SortPref {
                metric: Some(vec!["rows".into()]),
                descending: true
            }
        );
        let again = App::new(vec![Box::new(Fake(vec!["x"]))], path.clone());
        assert_eq!(again.sort_id(0).as_deref(), Some("rows"));
        fs::remove_file(&path).unwrap();
    }

    fn screen(app: &App, width: u16, height: u16) -> String {
        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| app.render(f)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn every_field_renders_with_the_sort_metric_marked_and_the_rest_scrolling() {
        let path = scratch("render");
        let _ = fs::remove_file(&path);
        let mut app = App::new(vec![Box::new(Fake(vec!["nick", "work"]))], path.clone());
        let soon = (chrono_now() + 3 * 3600).to_string();
        let response = |session: f64, rows: u32| {
            serde_json::json!({
                "five_hour": {"utilization": session, "resets_at": soon.parse::<i64>().unwrap()},
                "seven_day": {"utilization": 42.5},
                "limits": [
                    {"kind": "session", "percent": session, "severity": "normal"},
                    {"kind": "weekly_scoped", "percent": 78, "severity": "warning",
                     "scope": {"model": {"display_name": "Fable"}}}
                ],
                "extra_usage": {"used_credits": 12.5, "monthly_limit": 50, "is_enabled": true},
                "rows": rows,
                "organization": {"tier": "max"}
            })
        };
        app.usage.insert(
            (0, "nick".into()),
            UsageState::Ready(parse_usage(&response(60.0, 4521))),
        );
        app.usage.insert(
            (0, "work".into()),
            UsageState::Ready(parse_usage(&response(5.0, 9000))),
        );
        let wide = screen(&app, 110, 32);
        println!("{wide}");
        assert!(wide.contains("fake accounts (2) · ↑ five hour · utilization"));
        app.usage.insert(
            (0, "work".into()),
            UsageState::Ready(parse_usage(
                &serde_json::json!({"limits": [{"kind": "session", "percent": 5}]}),
            )),
        );
        app.usage.insert(
            (0, "nick".into()),
            UsageState::Ready(parse_usage(
                &serde_json::json!({"limits": [{"kind": "session", "percent": 60}]}),
            )),
        );
        let shared = screen(&app, 110, 32);
        assert!(shared.contains("fake accounts (2) · ↑ session · percent"));
        assert!(shared.contains("↑ session · percent "));
        assert!(!shared.contains("limits"));
        app.usage.insert(
            (0, "nick".into()),
            UsageState::Ready(parse_usage(&response(60.0, 4521))),
        );
        app.usage.insert(
            (0, "work".into()),
            UsageState::Ready(parse_usage(&response(5.0, 9000))),
        );
        let (nick_row, work_row) = (wide.find("nick").unwrap(), wide.find("work").unwrap());
        assert!(work_row < nick_row, "lower utilization sorts first");
        for expected in [
            "↑ five hour · utilization",
            "  seven day · utilization",
            "  limits · session · percent",
            "  limits · weekly scoped · percent",
            "  extra usage · used credits",
            "  extra usage · monthly limit",
            "  rows",
            "── details",
            "  limits · weekly scoped · scope · model · display name  Fable",
            "  limits · session · severity",
            "  organization · tier",
            "  extra usage · is enabled",
            "resets in 2h 59m",
            "4,521",
            "60%",
            "42.5%",
            "yes",
            "max",
        ] {
            assert!(wide.contains(expected), "missing {expected:?} in\n{wide}");
        }
        assert!(!wide.contains("12.50"), "trailing zeros trimmed");

        press(&mut app, KeyCode::Char('s'));
        let picker = screen(&app, 110, 32);
        println!("{picker}");
        assert!(picker.contains("sort fake accounts by"));
        assert!(picker.contains("▶ ↑ five hour · utilization"));
        assert!(picker.contains("rows"));
        press(&mut app, KeyCode::Esc);

        let narrow = screen(&app, 70, 18);
        println!("{narrow}");
        assert!(narrow.contains("pgup/pgdn"));
        assert!(!narrow.contains("resets in"));
        assert!(narrow.lines().all(|l| l.chars().count() <= 70));
        assert!(narrow.contains("1-"));
        press(&mut app, KeyCode::PageDown);
        let scrolled = screen(&app, 70, 18);
        println!("{scrolled}");
        assert!(scrolled.contains("── details"));
        assert!(!scrolled.contains("1-"));
        let _ = fs::remove_file(&path);
    }

    fn chrono_now() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
    }
}
