use std::cell::Cell;
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::Duration;

use accio_custom::{Definition, Draft, Registry};
use accio_provider::{
    common_prefix, contract, humanize_until, label, read_json, write_atomic, Account, Fetch, Knob,
    Metric, MetricValue, Outcome, Provider, Scale, Usage,
};
use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{prelude::*, widgets::*, DefaultTerminal};
use serde_json::{json, Map, Value};

const BAR: Color = Color::Cyan;
const SCROLL_STEP: usize = 5;

type Build = fn(&Registry) -> Result<Vec<Box<dyn Provider>>>;

enum UsageState {
    Loading,
    Ready(Usage),
    Error(String),
}

struct FetchMsg {
    generation: usize,
    provider: usize,
    account: String,
    outcome: Outcome,
}

enum Mode {
    Normal,
    AddMethod(usize),
    Form(Form),
    ConfirmDelete(String),
    ConfirmRemove(String),
    SortPicker(usize),
    Providers(usize),
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
    width: u16,
}

enum FormAction {
    Login,
    Configure { editing: Option<String> },
    NewProvider,
    Definition { name: String },
}

#[derive(Clone)]
struct Field {
    label: String,
    hint: String,
    value: String,
    secret: bool,
    // Filled rows of this kind spawn a blank one below
    grows: bool,
}

impl Field {
    fn new(label: &str, hint: &str) -> Self {
        Field {
            label: label.to_string(),
            hint: hint.to_string(),
            value: String::new(),
            secret: false,
            grows: false,
        }
    }

    fn row(label: &str, hint: &str) -> Self {
        let mut f = Field::new(label, hint);
        f.grows = true;
        f
    }

    fn with(mut self, value: &str) -> Self {
        self.value = value.to_string();
        self
    }

    fn knob(k: &Knob) -> Self {
        let mut f = Field::new(&k.name, &format!("{} (optional)", k.hint));
        f.secret = k.secret;
        f
    }

    fn extra() -> Self {
        Field::row("extra", "KEY=VALUE (optional)")
    }
}

impl Form {
    fn login(provider: &str) -> Self {
        Form {
            title: format!(" add {provider} account "),
            fields: vec![Field::new("name", "defaults to the account email")],
            focus: 0,
            action: FormAction::Login,
            width: 66,
        }
    }

    fn configure(provider: &str, knobs: &[Knob]) -> Self {
        let mut fields = vec![Field::new("name", "profile name")];
        fields.extend(knobs.iter().map(Field::knob));
        fields.push(Field::extra());
        Form {
            title: format!(" configure {provider} profile "),
            fields,
            focus: 0,
            action: FormAction::Configure { editing: None },
            width: 66,
        }
    }

    fn new_provider() -> Self {
        Form {
            title: " new provider ".to_string(),
            fields: vec![
                Field::new(
                    "name",
                    "lowercase letters, digits, - or _ (pi loads the bundled preset)",
                ),
                Field::new(
                    "command",
                    "program to run, blank keeps the preset or the name",
                ),
            ],
            focus: 0,
            action: FormAction::NewProvider,
            width: 80,
        }
    }

    // One row per list entry with a blank row under each list
    fn definition(draft: &Draft) -> Self {
        let rows = |label: &str, hint: &str, lines: &[String]| -> Vec<Field> {
            lines
                .iter()
                .map(|l| Field::row(label, hint).with(l))
                .chain([Field::row(label, hint)])
                .collect()
        };
        let mut fields = vec![
            Field::new("command", "program and arguments, defaults to the name")
                .with(&draft.command),
            Field::new("home", "the harness config directory, ~/ or absolute").with(&draft.home),
            Field::new("home_env", "variable that relocates home, sessions need it")
                .with(&draft.home_env),
            Field::new("copy", "comma separated, what a session takes from home").with(&draft.copy),
            Field::new("unset", "comma separated variables scrubbed from sessions")
                .with(&draft.unset),
        ];
        fields.extend(rows("env", "KEY=VALUE set in every session", &draft.env));
        fields.extend(rows(
            "knob",
            "NAME hint, a leading * masks the value",
            &draft.knobs,
        ));
        fields.extend(rows(
            "file",
            "path {json template}, merge: prefix deep merges",
            &draft.files,
        ));
        Form {
            title: format!(" define '{}' ", draft.name),
            fields,
            focus: 0,
            action: FormAction::Definition {
                name: draft.name.clone(),
            },
            width: 100,
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
            width: 66,
        }
    }

    // A filled row that ends its run spawns a blank one below
    fn grow(&mut self) {
        let mut i = 0;
        while i < self.fields.len() {
            let f = &self.fields[i];
            let ends_run = self
                .fields
                .get(i + 1)
                .is_none_or(|n| !n.grows || n.label != f.label);
            if f.grows && ends_run && !f.value.trim().is_empty() {
                let mut blank = f.clone();
                blank.value.clear();
                self.fields.insert(i + 1, blank);
                if i < self.focus {
                    self.focus += 1;
                }
            }
            i += 1;
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
    let prefs = prefs_path()?;
    let registry_dir = accio_custom::dir()?;
    let mut terminal = ratatui::init();
    let result = App::new(providers, prefs, registry_dir)
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
    let mut app = App::new(vec![provider], prefs_path()?, accio_custom::dir()?);
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
    registry_dir: PathBuf,
    build: Build,
    // What the providers overlay lists, refreshed on open and after every change
    definitions: Vec<Definition>,
    // Bumped when tabs are rebuilt so fetches for the old tabs are dropped
    generation: usize,
}

impl App {
    fn new(providers: Vec<Box<dyn Provider>>, prefs_path: PathBuf, registry_dir: PathBuf) -> Self {
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
            registry_dir,
            build: crate::providers_in,
            definitions: Vec::new(),
            generation: 0,
        }
    }

    fn registry(&self) -> Result<Registry> {
        Registry::load_at(self.registry_dir.clone())
    }

    // Tabs rebuilt from the registry, the named provider focused when it has a tab
    fn reload_providers(&mut self, focus: Option<&str>) {
        let registry = match self.registry() {
            Ok(r) => r,
            Err(e) => {
                self.status = format!("{e:#}");
                return;
            }
        };
        let providers = match (self.build)(&registry) {
            Ok(p) if !p.is_empty() => p,
            Ok(_) => {
                self.status = "every provider is disabled".into();
                return;
            }
            Err(e) => {
                self.status = format!("{e:#}");
                return;
            }
        };
        let current = focus
            .map(str::to_string)
            .unwrap_or_else(|| self.providers[self.tab].name().to_string());
        self.definitions = registry.definitions;
        self.providers = providers;
        self.tab = self
            .providers
            .iter()
            .position(|p| p.name() == current)
            .unwrap_or(0);
        self.selected = self
            .providers
            .iter()
            .map(|p| p.active().unwrap_or(0))
            .collect();
        self.usage.clear();
        self.usage_scroll.set(0);
        self.generation += 1;
        self.fetch_all();
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
        if let Mode::Providers(cursor) = self.mode {
            self.on_key_providers(cursor, key);
            return Ok(Action::None);
        }
        match &mut self.mode {
            Mode::Normal => return self.on_key_normal(key),
            Mode::SortPicker(_) | Mode::Providers(_) => {}
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
                    self.status = match &form.action {
                        FormAction::Definition { name } => format!("'{name}' left as written"),
                        _ => "cancelled".into(),
                    };
                    self.mode = Mode::Normal;
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
            Mode::ConfirmRemove(name) => {
                let name = name.clone();
                let cursor = self
                    .definitions
                    .iter()
                    .position(|d| d.name == name)
                    .unwrap_or(0);
                self.mode = Mode::Providers(cursor);
                if matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                    match self.registry().and_then(|r| r.remove(&name)) {
                        Ok(()) => {
                            self.status = format!("removed '{name}' - its profiles stay on disk");
                            self.reload_providers(None);
                            let last = self.definitions.len().saturating_sub(1);
                            self.mode = Mode::Providers(cursor.min(last));
                        }
                        Err(e) => self.status = format!("remove failed: {e:#}"),
                    }
                }
            }
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
        match &form.action {
            FormAction::Login => Ok(Action::Add(form.fields.first().and_then(field_value))),
            FormAction::NewProvider => {
                let name = form
                    .fields
                    .first()
                    .and_then(field_value)
                    .unwrap_or_default();
                let command = form.fields.get(1).and_then(field_value);
                match self
                    .registry()
                    .and_then(|r| r.add(&name, command.as_deref()))
                {
                    Ok(path) => {
                        self.status = format!("defined '{name}' in {}", contract(&path));
                        self.reload_providers(Some(&name));
                        match self.registry().and_then(|r| r.draft(&name)) {
                            Ok(draft) => self.mode = Mode::Form(Form::definition(&draft)),
                            Err(e) => self.status = format!("{e:#}"),
                        }
                    }
                    Err(e) => {
                        self.status = format!("new provider failed: {e:#}");
                        self.mode = Mode::Form(form);
                    }
                }
                Ok(Action::None)
            }
            FormAction::Definition { name } => {
                let name = name.clone();
                let mut draft = Draft {
                    name: name.clone(),
                    ..Draft::default()
                };
                for f in &form.fields {
                    let v = f.value.trim().to_string();
                    match f.label.as_str() {
                        "command" => draft.command = v,
                        "home" => draft.home = v,
                        "home_env" => draft.home_env = v,
                        "copy" => draft.copy = v,
                        "unset" => draft.unset = v,
                        _ if v.is_empty() => {}
                        "env" => draft.env.push(v),
                        "knob" => draft.knobs.push(v),
                        "file" => draft.files.push(v),
                        _ => {}
                    }
                }
                match self.registry().and_then(|r| r.save(&draft)) {
                    Ok(_) => {
                        self.status = format!("saved '{name}'");
                        self.reload_providers(Some(&name));
                    }
                    Err(e) => {
                        // The form stays up so the fix is a keystroke away
                        self.status = format!("save failed: {e:#}");
                        self.mode = Mode::Form(form);
                    }
                }
                Ok(Action::None)
            }
            FormAction::Configure { editing } => {
                let mut name = editing.clone();
                let mut values = BTreeMap::new();
                for f in &form.fields {
                    let Some(v) = field_value(f) else { continue };
                    if f.grows {
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
                self.mode = if !p.can_login() {
                    Mode::Form(Form::configure(p.name(), &p.knobs()))
                } else if p.knobs().is_empty() {
                    Mode::Form(Form::login(p.name()))
                } else {
                    Mode::AddMethod(0)
                };
            }
            KeyCode::Char('p') if !self.session_picker => match self.registry() {
                Ok(registry) => {
                    let current = self.providers[self.tab].name();
                    let cursor = registry
                        .definitions
                        .iter()
                        .position(|d| d.name == current)
                        .unwrap_or(0);
                    self.definitions = registry.definitions;
                    self.mode = Mode::Providers(cursor);
                }
                Err(e) => self.status = format!("{e:#}"),
            },
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

    fn on_key_providers(&mut self, cursor: usize, key: KeyEvent) {
        let count = self.definitions.len();
        let cursor = cursor.min(count.saturating_sub(1));
        let def = self.definitions.get(cursor).cloned();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('p') => self.mode = Mode::Normal,
            KeyCode::Up | KeyCode::Char('k') => {
                self.mode = Mode::Providers(cursor.saturating_sub(1));
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.mode = Mode::Providers((cursor + 1).min(count.saturating_sub(1)));
            }
            KeyCode::Char('n') => self.mode = Mode::Form(Form::new_provider()),
            KeyCode::Enter | KeyCode::Char(' ') => {
                let Some(def) = def else { return };
                let enabled = self.definitions.iter().filter(|d| !d.disabled).count();
                if !def.disabled && enabled <= 1 {
                    self.status = "one provider has to stay enabled".into();
                    return;
                }
                match self
                    .registry()
                    .and_then(|r| r.set_disabled(&def.name, !def.disabled))
                {
                    Ok(()) => {
                        let verb = if def.disabled { "enabled" } else { "disabled" };
                        self.status = format!("{verb} '{}'", def.name);
                        self.reload_providers(None);
                    }
                    Err(e) => self.status = format!("{e:#}"),
                }
            }
            KeyCode::Char('e') => {
                let Some(def) = def else { return };
                match self.registry().and_then(|r| r.draft(&def.name)) {
                    Ok(draft) => self.mode = Mode::Form(Form::definition(&draft)),
                    Err(e) => self.status = format!("{e:#}"),
                }
            }
            KeyCode::Char('d') => {
                let Some(def) = def else { return };
                if def.builtin {
                    self.status = format!("'{}' is built in - disable it instead", def.name);
                } else {
                    self.mode = Mode::ConfirmRemove(def.name);
                }
            }
            _ => {}
        }
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
                let generation = self.generation;
                std::thread::spawn(move || {
                    let _ = tx.send(FetchMsg {
                        generation,
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
            if msg.generation != self.generation {
                continue;
            }
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
        let info = self.providers[self.tab].info();
        let info_height = if info.is_empty() {
            0
        } else {
            info.len() as u16 + 2
        };
        let [tabs_area, accounts_area, info_area, usage_area, footer_area] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(list_height),
            Constraint::Length(info_height),
            Constraint::Min(4),
            Constraint::Length(self.footer_height(f.area().width)),
        ])
        .areas(f.area());
        self.render_tabs(f, tabs_area);
        self.render_accounts(f, accounts_area);
        if !info.is_empty() {
            render_info(f, info_area, &info);
        }
        self.render_usage(f, usage_area);
        self.render_footer(f, footer_area);
        match &self.mode {
            Mode::AddMethod(selected) => self.render_menu(f, *selected),
            Mode::Form(form) => render_form(f, form),
            Mode::SortPicker(cursor) => self.render_sort_picker(f, *cursor),
            Mode::Providers(_) | Mode::ConfirmRemove(_) => {
                self.render_providers(f, self.providers_cursor());
            }
            _ => {}
        }
    }

    // Overlay cursor, the confirm prompt keeps the row it asks about
    fn providers_cursor(&self) -> usize {
        match &self.mode {
            Mode::Providers(cursor) => *cursor,
            Mode::ConfirmRemove(name) => self
                .definitions
                .iter()
                .position(|d| d.name == *name)
                .unwrap_or(0),
            _ => 0,
        }
    }

    fn render_providers(&self, f: &mut Frame, cursor: usize) {
        let defs = &self.definitions;
        let name_w = defs
            .iter()
            .map(|d| d.name.chars().count())
            .max()
            .unwrap_or(6)
            .clamp(6, 20);
        let path_w = 40;
        let items: Vec<ListItem> = defs
            .iter()
            .map(|d| {
                let (state, style) = if d.disabled {
                    ("disabled", Style::new().dim())
                } else {
                    ("enabled ", Style::new().green())
                };
                let kind = if d.builtin { "built in" } else { "custom  " };
                let detail = if d.builtin {
                    String::new()
                } else {
                    contract(&d.path)
                };
                ListItem::new(Line::from(vec![
                    Span::styled(
                        pad(&d.name, name_w),
                        if d.disabled {
                            Style::new().dim()
                        } else {
                            Style::new()
                        },
                    ),
                    Span::raw("  "),
                    Span::styled(state.to_string(), style),
                    Span::raw("  "),
                    Span::styled(kind.to_string(), Style::new().dim()),
                    Span::raw("  "),
                    Span::styled(tail(&detail, path_w), Style::new().dim()),
                ]))
            })
            .collect();
        let rect = centered(
            f.area(),
            (name_w + 8 + 8 + path_w + 14) as u16,
            items.len() as u16 + 5,
        );
        f.render_widget(Clear, rect);
        let block = panel(" providers ".to_string());
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
                    "enter enable/disable · n new · e edit · d remove · esc close",
                    Style::new().dim(),
                ),
            ]),
            hint_area,
        );
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
            (" usage ".to_string(), Vec::new())
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

    fn help(&self) -> &'static str {
        if self.session_picker {
            "↑/↓ select · enter launch session · s/S sort · r refresh · q/esc cancel"
        } else {
            "←/→ provider · ↑/↓ select · enter switch · s/S sort · a add · e edit · d delete · p providers · r refresh · q quit"
        }
    }

    // Status line plus however many lines the help wraps onto
    fn footer_height(&self, width: u16) -> u16 {
        let usable = width.saturating_sub(4).max(1) as usize;
        1 + self.help().chars().count().div_ceil(usable) as u16
    }

    fn render_footer(&self, f: &mut Frame, area: Rect) {
        let top_line = match &self.mode {
            Mode::ConfirmDelete(name) => Line::styled(
                format!("delete '{name}' from accio? (y/N)"),
                Style::new().yellow().bold(),
            ),
            Mode::ConfirmRemove(name) => Line::styled(
                format!("remove provider '{name}'? its profiles stay on disk (y/N)"),
                Style::new().yellow().bold(),
            ),
            _ => Line::styled(self.status.clone(), Style::new().cyan()),
        };
        let [status_area, help_area] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(area);
        let padded = || Block::new().padding(Padding::horizontal(2));
        f.render_widget(Paragraph::new(top_line).block(padded()), status_area);
        f.render_widget(
            Paragraph::new(Line::styled(self.help(), Style::new().dim()))
                .wrap(Wrap { trim: true })
                .block(padded()),
            help_area,
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
    let rect = centered(f.area(), form.width, form.fields.len() as u16 + 4);
    f.render_widget(Clear, rect);
    let block = panel(form.title.clone());
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    let [fields_area, hint_area] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(2)]).areas(inner);
    // marker, label, the gap after it and the cursor
    let value_w = (inner.width as usize)
        .saturating_sub(2 + label_w + 2 + 1)
        .max(8);

    let lines: Vec<Line> = form
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
                // The focused row shows where typing lands, the rest their start
                spans.push(Span::raw(if focused {
                    tail(&shown, value_w)
                } else {
                    clip(&shown, value_w)
                }));
            }
            if focused {
                spans.push(Span::styled("▏", Style::new().dim()));
            }
            Line::from(spans)
        })
        .collect();
    let visible = fields_area.height as usize;
    let scroll = (form.focus + 1).saturating_sub(visible);
    f.render_widget(
        Paragraph::new(lines).scroll((scroll as u16, 0)),
        fields_area,
    );
    f.render_widget(
        Paragraph::new(vec![
            Line::raw(""),
            Line::styled(
                "↑/↓ field · enter next, saves on last · esc cancel",
                Style::new().dim(),
            ),
        ]),
        hint_area,
    );
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

// Where the tab's files live and what it holds, straight from the provider
fn render_info(f: &mut Frame, area: Rect, info: &[(String, String)]) {
    let width = area.width.saturating_sub(4) as usize;
    let label_w = info
        .iter()
        .map(|(l, _)| l.chars().count())
        .max()
        .unwrap_or(4)
        .clamp(4, 12);
    let value_w = width.saturating_sub(label_w + 2).max(8);
    let lines: Vec<Line> = info
        .iter()
        .map(|(l, v)| {
            Line::from(vec![
                Span::styled(pad(l, label_w), Style::new().dim()),
                Span::raw("  "),
                Span::raw(clip(v, value_w)),
            ])
        })
        .collect();
    f.render_widget(
        Paragraph::new(lines).block(panel(" info ".to_string())),
        area,
    );
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
    if eighths / 8 < width && !eighths.is_multiple_of(8) {
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

    // A provider under any name, with or without a login command
    struct Named(String, bool);
    impl Provider for Named {
        fn name(&self) -> &str {
            &self.0
        }
        fn accounts(&self) -> Vec<Account> {
            Vec::new()
        }
        fn active(&self) -> Option<usize> {
            None
        }
        fn activate(&mut self, _: usize) -> Result<()> {
            panic!("nothing to activate")
        }
        fn delete(&mut self, _: &str) -> Result<()> {
            panic!("nothing to delete")
        }
        fn add(&mut self, _: Option<&str>) -> Result<String> {
            panic!("no login command")
        }
        fn fetches(&self) -> Vec<Fetch> {
            Vec::new()
        }
        fn can_login(&self) -> bool {
            self.1
        }
        fn info(&self) -> Vec<(String, String)> {
            vec![
                ("profiles".into(), format!("~/accio/accounts/{}", self.0)),
                (
                    "login".into(),
                    if self.1 { self.0.clone() } else { "-".into() },
                ),
            ]
        }
    }

    #[test]
    fn info_panel_lists_what_the_provider_reports() {
        let app = App::new(
            vec![Box::new(Named("zed".into(), false))],
            scratch("info"),
            no_registry(),
        );
        let shown = screen(&app, 80, 20);
        println!("{shown}");
        assert!(shown.contains("┌ info "));
        assert!(shown.contains("profiles  ~/accio/accounts/zed"));
        assert!(shown.contains("login     -"));
        assert!(shown.contains("┌ usage "));
        let bare = App::new(
            vec![Box::new(Fake(vec!["x"]))],
            scratch("info-bare"),
            no_registry(),
        );
        assert!(!screen(&bare, 80, 20).contains("┌ info "));
    }

    fn fake_build(registry: &Registry) -> Result<Vec<Box<dyn Provider>>> {
        Ok(registry
            .enabled()
            .map(|d| Box::new(Named(d.name.clone(), d.builtin)) as Box<dyn Provider>)
            .collect())
    }

    fn scratch(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("accio-tui-{}-{name}.json", std::process::id()))
    }

    fn scratch_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("accio-tui-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn no_registry() -> PathBuf {
        std::env::temp_dir().join("accio-tui-no-registry")
    }

    fn press(app: &mut App, code: KeyCode) -> Action {
        app.on_key(KeyEvent::new(code, KeyModifiers::NONE)).unwrap()
    }

    fn type_text(app: &mut App, text: &str) {
        for c in text.chars() {
            press(app, KeyCode::Char(c));
        }
    }

    #[test]
    fn custom_provider_without_knobs_gets_a_configure_form() {
        let mut app = App::new(
            vec![Box::new(Named("zed".into(), false))],
            scratch("noknobs"),
            no_registry(),
        );
        press(&mut app, KeyCode::Char('a'));
        let Mode::Form(form) = &app.mode else {
            panic!("configure form expected")
        };
        assert!(matches!(
            form.action,
            FormAction::Configure { editing: None }
        ));
        assert_eq!(form.fields[0].label, "name");
        assert!(form.fields[1].grows);
    }

    #[test]
    fn providers_overlay_defines_edits_toggles_and_removes() {
        let dir = scratch_dir("providers");
        let registry = Registry::load_at(dir.clone()).unwrap();
        let mut app = App::new(
            fake_build(&registry).unwrap(),
            scratch("providers"),
            dir.clone(),
        );
        app.build = fake_build;
        assert_eq!(app.providers.len(), 4);

        press(&mut app, KeyCode::Char('p'));
        assert!(matches!(app.mode, Mode::Providers(0)));
        let shown = screen(&app, 100, 30);
        println!("{shown}");
        assert!(shown.contains("providers"));
        assert!(shown.contains("▶ claude"));
        assert!(shown.contains("built in"));
        assert!(shown.contains("enabled"));

        // A new provider from the bundled preset lands in its definition form
        press(&mut app, KeyCode::Char('n'));
        type_text(&mut app, "pi");
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Enter);
        assert!(dir.join("pi.json").exists());
        assert_eq!(app.providers.len(), 5);
        assert_eq!(app.providers[app.tab].name(), "pi");
        let Mode::Form(form) = &app.mode else {
            panic!("definition form expected")
        };
        assert!(matches!(form.action, FormAction::Definition { .. }));
        assert_eq!(form.fields[0].value, "pi");
        assert_eq!(form.fields[1].value, "~/.pi/agent");
        let knobs: Vec<&str> = form
            .fields
            .iter()
            .filter(|f| f.label == "knob")
            .map(|f| f.value.as_str())
            .collect();
        assert_eq!(knobs.len(), 8);
        assert_eq!(knobs[3], "*API_KEY bearer token for that endpoint");
        assert_eq!(knobs[7], "");
        let shown = screen(&app, 100, 30);
        println!("{shown}");
        assert!(shown.contains("define 'pi'"));
        assert!(shown.contains("merge:models.json"));
        assert!(shown.contains("merge:settings.json"));

        // The command is rewritten, enter walks to the end and saves
        press(&mut app, KeyCode::Backspace);
        press(&mut app, KeyCode::Backspace);
        type_text(&mut app, "pi --fast");
        let mut steps = 0;
        while matches!(app.mode, Mode::Form(_)) {
            press(&mut app, KeyCode::Enter);
            steps += 1;
            assert!(steps < 40, "form never submitted");
        }
        assert_eq!(app.status, "saved 'pi'");
        let saved: Value =
            serde_json::from_str(&fs::read_to_string(dir.join("pi.json")).unwrap()).unwrap();
        assert_eq!(saved["command"], json!(["pi", "--fast"]));
        assert_eq!(saved["knobs"].as_array().unwrap().len(), 7);
        assert!(saved["files"]["merge:models.json"].is_object());
        assert_eq!(app.providers[app.tab].name(), "pi");

        // A broken edit keeps the form up with the error in the status line
        press(&mut app, KeyCode::Char('p'));
        assert!(matches!(app.mode, Mode::Providers(4)));
        press(&mut app, KeyCode::Char('e'));
        assert!(matches!(app.mode, Mode::Form(_)));
        press(&mut app, KeyCode::Down);
        for _ in 0..11 {
            press(&mut app, KeyCode::Backspace);
        }
        type_text(&mut app, "relative");
        while matches!(app.mode, Mode::Form(ref f) if f.focus + 1 < f.fields.len()) {
            press(&mut app, KeyCode::Enter);
        }
        press(&mut app, KeyCode::Enter);
        assert!(app.status.starts_with("save failed: home must start with"));
        assert!(matches!(app.mode, Mode::Form(_)));
        press(&mut app, KeyCode::Esc);

        // Toggles, custom and built in alike
        press(&mut app, KeyCode::Char('p'));
        assert!(matches!(app.mode, Mode::Providers(4)));
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.status, "disabled 'pi'");
        assert_eq!(app.providers.len(), 4);
        assert!(screen(&app, 100, 30).contains("disabled"));
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.status, "enabled 'pi'");
        assert_eq!(app.providers.len(), 5);
        for _ in 0..4 {
            press(&mut app, KeyCode::Char('k'));
        }
        assert!(matches!(app.mode, Mode::Providers(0)));
        press(&mut app, KeyCode::Enter);
        assert!(dir.join("claude.json").exists());
        assert_eq!(app.providers.len(), 4);
        assert_eq!(app.providers[0].name(), "codex");
        press(&mut app, KeyCode::Enter);
        assert!(!dir.join("claude.json").exists());
        assert_eq!(app.providers.len(), 5);

        // Built ins only switch off
        press(&mut app, KeyCode::Char('d'));
        assert!(app.status.contains("built in"));
        assert!(matches!(app.mode, Mode::Providers(0)));
        press(&mut app, KeyCode::Char('e'));
        assert!(app.status.contains("built in"));
        assert!(matches!(app.mode, Mode::Providers(0)));

        // The last enabled provider stays on
        for _ in 0..4 {
            press(&mut app, KeyCode::Char('j'));
            press(&mut app, KeyCode::Enter);
        }
        assert_eq!(app.providers.len(), 1);
        for _ in 0..4 {
            press(&mut app, KeyCode::Char('k'));
        }
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.status, "one provider has to stay enabled");
        assert_eq!(app.providers.len(), 1);
        for _ in 0..4 {
            press(&mut app, KeyCode::Char('j'));
        }
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.providers.len(), 2);

        // Removal asks first, then leaves the overlay on a valid row
        press(&mut app, KeyCode::Char('d'));
        assert!(matches!(app.mode, Mode::ConfirmRemove(ref n) if n == "pi"));
        assert!(screen(&app, 100, 30).contains("remove provider 'pi'?"));
        press(&mut app, KeyCode::Char('n'));
        assert!(matches!(app.mode, Mode::Providers(4)));
        assert!(dir.join("pi.json").exists());
        press(&mut app, KeyCode::Char('d'));
        press(&mut app, KeyCode::Char('y'));
        assert!(!dir.join("pi.json").exists());
        assert!(app.status.starts_with("removed 'pi'"));
        assert!(matches!(app.mode, Mode::Providers(3)));
        assert_eq!(app.providers.len(), 1);
        press(&mut app, KeyCode::Esc);
        assert!(matches!(app.mode, Mode::Normal));

        let mut picker = App::new(
            vec![Box::new(Named("claude".into(), true))],
            scratch("providers-picker"),
            dir.clone(),
        );
        picker.session_picker = true;
        press(&mut picker, KeyCode::Char('p'));
        assert!(matches!(picker.mode, Mode::Normal));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn session_picker_selects_even_active_profile_and_cancels_without_mutating() {
        let mut app = App::new(
            vec![Box::new(Fake(vec!["work"]))],
            scratch("picker"),
            no_registry(),
        );
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
        let mut app = App::new(
            vec![Box::new(Fake(vec!["a", "b", "c", "d"]))],
            path.clone(),
            no_registry(),
        );
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
        let mut empty = App::new(
            vec![Box::new(Fake(vec!["lonely"]))],
            scratch("empty"),
            no_registry(),
        );
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
        let again = App::new(vec![Box::new(Fake(vec!["x"]))], path.clone(), no_registry());
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
        let mut app = App::new(
            vec![Box::new(Fake(vec!["nick", "work"]))],
            path.clone(),
            no_registry(),
        );
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
