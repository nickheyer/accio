use std::cell::Cell;
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};

use accio_custom::{Definition, Draft, Preset, Registry};
use accio_provider::{
    common_prefix, contract, humanize_until, label, local_stamp, read_json, sanitize_name,
    write_atomic, Account, Fetch, Knob, Metric, MetricValue, Outcome, Provider, Scale, Session,
    Until, Usage,
};
use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{prelude::*, widgets::*, DefaultTerminal};
use serde_json::{json, Map, Value};

use crate::hold::{self, Hold, Wait};

const BAR: Color = Color::Cyan;
const SCROLL_STEP: usize = 5;
const POLL: Duration = Duration::from_secs(1);

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
    ConfirmQuit,
    Metrics(usize),
    Providers(usize),
    Sessions(Preview),
}

// A change to the login every plain session of a provider runs on
#[derive(Clone, Debug, PartialEq)]
enum Change {
    Switch(String),
    Login(Option<String>),
    Edit {
        name: String,
        values: BTreeMap<String, String>,
    },
}

impl Change {
    fn words(&self) -> String {
        match self {
            Change::Switch(name) => format!("switch to '{name}'"),
            Change::Login(_) => "new login".to_string(),
            Change::Edit { name, .. } => format!("edit of '{name}'"),
        }
    }

    fn does(&self) -> String {
        match self {
            Change::Switch(name) => format!("switches to '{name}'"),
            Change::Login(_) => "starts the login".to_string(),
            Change::Edit { name, .. } => format!("saves '{name}'"),
        }
    }
}

// Live sessions a proposed change lands on, each with when the change may reach it
struct Preview {
    change: Change,
    sessions: Vec<Session>,
    waits: Vec<Wait>,
    cursor: usize,
}

impl Preview {
    fn holds(&self) -> Vec<Hold> {
        self.sessions
            .iter()
            .zip(&self.waits)
            .filter(|(_, wait)| **wait != Wait::Now)
            .map(|(session, wait)| Hold {
                id: session.now.id.clone(),
                name: session.name.clone(),
                wait: *wait,
            })
            .collect()
    }

    // What the listed sessions still have to do before the change is made
    fn until(&self) -> String {
        let states: Vec<_> = self.sessions.iter().map(|s| s.now.clone()).collect();
        hold::until(&self.holds(), &states)
    }
}

// A confirmed change parked until the sessions it waits on let it through
struct Held {
    provider: String,
    change: Change,
    holds: Vec<Hold>,
    until: String,
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
    // Metric paths each provider shows as account columns, none saved means the default set
    columns: BTreeMap<String, Vec<Vec<String>>>,
    hide_info: bool,
}

// A metric path stored as an array of its segments
fn segments(v: &Value) -> Option<Vec<String>> {
    let path: Vec<String> = v
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect();
    (!path.is_empty()).then_some(path)
}

impl Prefs {
    fn load(path: &Path) -> Self {
        let mut prefs = Prefs::default();
        let Some(v) = read_json(path) else {
            return prefs;
        };
        if let Some(sort) = v.get("sort").and_then(Value::as_object) {
            for (provider, s) in sort {
                prefs.sort.insert(
                    provider.clone(),
                    SortPref {
                        metric: s.get("metric").and_then(segments),
                        descending: s
                            .get("descending")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    },
                );
            }
        }
        if let Some(columns) = v.get("columns").and_then(Value::as_object) {
            for (provider, list) in columns {
                if let Some(list) = list.as_array() {
                    prefs
                        .columns
                        .insert(provider.clone(), list.iter().filter_map(segments).collect());
                }
            }
        }
        prefs.hide_info = v.get("info").and_then(Value::as_bool) == Some(false);
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
            serde_json::to_string_pretty(&json!({
                "sort": sort,
                "columns": self.columns,
                "info": !self.hide_info,
            }))?
            .as_bytes(),
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
    FromPreset,
    Definition { name: String },
}

#[derive(Clone)]
struct Field {
    label: String,
    hint: String,
    value: String,
    secret: bool,
    grows: bool,
    options: Vec<(String, String)>,
}

impl Field {
    fn new(label: &str, hint: &str) -> Self {
        Field {
            label: label.to_string(),
            hint: hint.to_string(),
            value: String::new(),
            secret: false,
            grows: false,
            options: Vec::new(),
        }
    }

    fn choice(label: &str, options: Vec<(String, String)>) -> Self {
        let mut f = Field::new(label, "nothing to pick from");
        f.value = options.first().map(|(v, _)| v.clone()).unwrap_or_default();
        f.options = options;
        f
    }

    fn is_choice(&self) -> bool {
        !self.options.is_empty()
    }

    fn chosen(&self) -> usize {
        self.options
            .iter()
            .position(|(v, _)| *v == self.value)
            .unwrap_or(0)
    }

    fn cycle(&mut self, back: bool) {
        let n = self.options.len();
        if n == 0 {
            return;
        }
        let i = self.chosen();
        let next = if back { (i + n - 1) % n } else { (i + 1) % n };
        self.value = self.options[next].0.clone();
    }

    // Typing a letter lands on the next option starting with it
    fn jump(&mut self, c: char) {
        let n = self.options.len();
        let i = self.chosen();
        let hit = (1..=n)
            .map(|k| (i + k) % n)
            .find(|&k| self.options[k].0.starts_with(c));
        if let Some(k) = hit {
            self.value = self.options[k].0.clone();
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
                Field::new("name", "lowercase letters, digits, - or _"),
                Field::new("command", "program to run, defaults to the name"),
            ],
            focus: 0,
            action: FormAction::NewProvider,
            width: 80,
        }
    }

    fn from_preset(presets: &[Preset]) -> Self {
        let options = presets
            .iter()
            .map(|p| {
                let detail = match &p.path {
                    Some(path) => contract(path),
                    None => "bundled".to_string(),
                };
                (p.name.clone(), detail)
            })
            .collect();
        Form {
            title: " new provider from preset ".to_string(),
            fields: vec![
                Field::choice("preset", options),
                Field::new("name", "defaults to the preset name"),
            ],
            focus: 0,
            action: FormAction::FromPreset,
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
    Add(usize, Option<String>),
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
    held: Option<Held>,
    polled: Instant,
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
            held: None,
            polled: Instant::now(),
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
            let due = self.tick();
            self.act(due, terminal);
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
                action => self.act(action, terminal),
            }
        }
    }

    // Runs a provider's own login with the TUI out of the way, then lands on the account it added
    fn act(&mut self, action: Action, terminal: &mut DefaultTerminal) {
        let Action::Add(tab, name) = action else {
            return;
        };
        ratatui::restore();
        let outcome = self.providers[tab].add(name.as_deref());
        *terminal = ratatui::init();
        match outcome {
            Ok(msg) => {
                self.status = msg;
                self.selected[tab] = self.providers[tab].active().unwrap_or(0);
                self.fetch_all();
            }
            Err(e) => self.status = format!("add failed: {e:#}"),
        }
    }

    fn tick(&mut self) -> Action {
        if self.polled.elapsed() < POLL {
            return Action::None;
        }
        self.polled = Instant::now();
        self.poll()
    }

    // Refreshes an open preview and makes a held change once nothing holds it back
    fn poll(&mut self) -> Action {
        self.refresh_preview();
        let Some(held) = &mut self.held else {
            return Action::None;
        };
        let Some(tab) = self
            .providers
            .iter()
            .position(|p| p.name() == held.provider)
        else {
            self.status = format!(
                "{} dropped, {} has no tab any more",
                held.change.words(),
                held.provider
            );
            self.held = None;
            return Action::None;
        };
        held.until = hold::until(&held.holds, &self.providers[tab].session_states());
        if !held.until.is_empty() {
            return Action::None;
        }
        match self.held.take() {
            Some(held) => self.apply(tab, held.change),
            None => Action::None,
        }
    }

    // Keeps an open preview in step with sessions starting, closing and changing state
    fn refresh_preview(&mut self) {
        let Mode::Sessions(preview) = &self.mode else {
            return;
        };
        let tab = self.tab;
        let states = self.providers[tab].session_states();
        let known = |id: &str| preview.sessions.iter().any(|s| s.now.id == id);
        let relisted = (states.len() != preview.sessions.len()
            || !states.iter().all(|state| known(&state.id)))
        .then(|| self.providers[tab].sessions(self.target(tab, &preview.change)));
        let Mode::Sessions(preview) = &mut self.mode else {
            return;
        };
        let Some(sessions) = relisted else {
            for session in &mut preview.sessions {
                if let Some(state) = states.iter().find(|state| state.id == session.now.id) {
                    session.now = state.clone();
                }
            }
            return;
        };
        let place = |old: &[Session], id: &str| old.iter().position(|s| s.now.id == id);
        let waits = sessions
            .iter()
            .map(|s| match place(&preview.sessions, &s.now.id) {
                Some(i) => preview.waits[i],
                None => Wait::initial(&s.now),
            })
            .collect();
        let cursor = preview
            .sessions
            .get(preview.cursor)
            .and_then(|focused| place(&sessions, &focused.now.id))
            .unwrap_or(preview.cursor);
        preview.cursor = cursor.min(sessions.len().saturating_sub(1));
        preview.sessions = sessions;
        preview.waits = waits;
    }

    // Account a change aims the live login at, none for a fresh login
    fn target(&self, tab: usize, change: &Change) -> Option<usize> {
        let name = match change {
            Change::Switch(name) | Change::Edit { name, .. } => name,
            Change::Login(_) => return None,
        };
        self.providers[tab]
            .accounts()
            .iter()
            .position(|a| a.name == *name)
    }

    // Opens the preview when sessions run on the live login, makes the change at once when none do
    fn propose(&mut self, change: Change) -> Action {
        let tab = self.tab;
        let sessions = self.providers[tab].sessions(self.target(tab, &change));
        if sessions.is_empty() {
            return self.apply(tab, change);
        }
        let waits = sessions.iter().map(|s| Wait::initial(&s.now)).collect();
        self.mode = Mode::Sessions(Preview {
            change,
            sessions,
            waits,
            cursor: 0,
        });
        Action::None
    }

    // Makes the previewed change, or parks it while a session it waits on holds it back
    fn confirm(&mut self) -> Action {
        let preview = match std::mem::replace(&mut self.mode, Mode::Normal) {
            Mode::Sessions(preview) => preview,
            other => {
                self.mode = other;
                return Action::None;
            }
        };
        let tab = self.tab;
        let holds = preview.holds();
        let until = hold::until(&holds, &self.providers[tab].session_states());
        if until.is_empty() {
            return self.apply(tab, preview.change);
        }
        self.status = format!("{} held", preview.change.words());
        self.held = Some(Held {
            provider: self.providers[tab].name().to_string(),
            change: preview.change,
            holds,
            until,
        });
        Action::None
    }

    // Makes a change on a tab's provider, a login comes back as the action that runs it
    fn apply(&mut self, tab: usize, change: Change) -> Action {
        if self
            .held
            .as_ref()
            .is_some_and(|held| held.provider == self.providers[tab].name())
        {
            self.held = None;
        }
        match change {
            Change::Login(name) => return Action::Add(tab, name),
            Change::Switch(name) => {
                let p = &mut self.providers[tab];
                let idx = p.accounts().iter().position(|a| a.name == name);
                self.status = match idx.map(|idx| (idx, p.activate(idx))) {
                    Some((idx, Ok(()))) => {
                        self.selected[tab] = p.active().unwrap_or(idx);
                        format!("switched to '{name}'")
                    }
                    Some((_, Err(e))) => format!("switch failed: {e:#}"),
                    None => format!("switch failed: no account named '{name}'"),
                };
            }
            Change::Edit { name, values } => self.configure(tab, Some(&name), &values),
        }
        Action::None
    }

    // Stores a configured profile and lands the selection on it
    fn configure(&mut self, tab: usize, name: Option<&str>, values: &BTreeMap<String, String>) {
        match self.providers[tab].configure(name, values) {
            Ok(msg) => {
                // Land the selection on the profile the message names
                if let Some(n) = msg.split('\'').nth(1) {
                    if let Some(i) = self.providers[tab]
                        .accounts()
                        .iter()
                        .position(|a| a.name == n)
                    {
                        self.selected[tab] = i;
                    }
                }
                self.status = msg;
                self.fetch_all();
            }
            Err(e) => self.status = format!("configure failed: {e:#}"),
        }
    }

    fn on_key_sessions(&mut self, key: KeyEvent) -> Action {
        let Mode::Sessions(preview) = &mut self.mode else {
            return Action::None;
        };
        let last = preview.sessions.len().saturating_sub(1);
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.status = format!("{} cancelled", preview.change.words());
                self.mode = Mode::Normal;
            }
            KeyCode::Up | KeyCode::Char('k') => preview.cursor = preview.cursor.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => preview.cursor = (preview.cursor + 1).min(last),
            KeyCode::Char(' ') => {
                if let Some(wait) = preview.waits.get_mut(preview.cursor) {
                    *wait = wait.next();
                }
            }
            KeyCode::Enter => return self.confirm(),
            _ => {}
        }
        Action::None
    }

    fn on_key(&mut self, key: KeyEvent) -> Result<Action> {
        if let Mode::Metrics(cursor) = self.mode {
            self.on_key_picker(cursor, key);
            return Ok(Action::None);
        }
        if let Mode::Providers(cursor) = self.mode {
            self.on_key_providers(cursor, key);
            return Ok(Action::None);
        }
        if let Mode::Sessions(_) = self.mode {
            return Ok(self.on_key_sessions(key));
        }
        match &mut self.mode {
            Mode::Normal => return self.on_key_normal(key),
            Mode::Metrics(_) | Mode::Providers(_) | Mode::Sessions(_) => {}
            Mode::ConfirmQuit => {
                self.mode = Mode::Normal;
                if matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                    return Ok(Action::Quit);
                }
                self.status = "still here".into();
            }
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
                KeyCode::Left | KeyCode::Right | KeyCode::Char(' ')
                    if form.fields[form.focus].is_choice() =>
                {
                    form.fields[form.focus].cycle(key.code == KeyCode::Left);
                }
                KeyCode::Backspace => {
                    let field = &mut form.fields[form.focus];
                    if !field.is_choice() {
                        field.value.pop();
                    }
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    let field = &mut form.fields[form.focus];
                    if field.is_choice() {
                        field.jump(c);
                    } else {
                        field.value.push(c);
                    }
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

    // Enter sorts by the metric or flips it when already in charge, space shows or hides its column
    fn on_key_picker(&mut self, cursor: usize, key: KeyEvent) {
        let entries = self.picker_entries(self.tab);
        let last = entries.len().saturating_sub(1);
        let cursor = cursor.min(last);
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('s') => self.mode = Mode::Normal,
            KeyCode::Up | KeyCode::Char('k') => {
                self.mode = Mode::Metrics(cursor.saturating_sub(1));
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.mode = Mode::Metrics((cursor + 1).min(last));
            }
            KeyCode::Char(' ') => {
                if let Some(path) = entries.get(cursor) {
                    self.toggle_column(path.clone());
                }
            }
            KeyCode::Enter => {
                self.mode = Mode::Normal;
                if let Some(path) = entries.get(cursor) {
                    let pref = self.sort_pref(self.tab);
                    let flip = self.sort_path(self.tab).as_ref() == Some(path);
                    self.set_sort(path.clone(), pref.descending != flip);
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
            FormAction::Login => {
                Ok(self.propose(Change::Login(form.fields.first().and_then(field_value))))
            }
            FormAction::NewProvider | FormAction::FromPreset => {
                let first = form
                    .fields
                    .first()
                    .and_then(field_value)
                    .unwrap_or_default();
                let second = form.fields.get(1).and_then(field_value);
                let manual = matches!(form.action, FormAction::NewProvider);
                let name = if manual {
                    first.clone()
                } else {
                    second.clone().unwrap_or_else(|| first.clone())
                };
                let written = self.registry().and_then(|r| {
                    if manual {
                        r.add(&name, second.as_deref())
                    } else {
                        r.add_from_preset(&first, &name)
                    }
                });
                match written {
                    Ok(path) => self.defined(&name, &path),
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
                // Rewriting the active profile changes the login live sessions run on
                let p = &self.providers[self.tab];
                let active = p
                    .active()
                    .and_then(|i| p.accounts().get(i).map(|a| a.name.clone()));
                let live =
                    active.filter(|a| name.as_deref().map(sanitize_name).as_ref() == Some(a));
                if let Some(name) = live {
                    return Ok(self.propose(Change::Edit { name, values }));
                }
                self.configure(self.tab, name.as_deref(), &values);
                Ok(Action::None)
            }
        }
    }

    // A freshly written definition opens in its edit form
    fn defined(&mut self, name: &str, path: &Path) {
        self.status = format!("defined '{name}' in {}", contract(path));
        self.reload_providers(Some(name));
        match self.registry().and_then(|r| r.draft(name)) {
            Ok(draft) => self.mode = Mode::Form(Form::definition(&draft)),
            Err(e) => self.status = format!("{e:#}"),
        }
    }

    fn on_key_normal(&mut self, key: KeyEvent) -> Result<Action> {
        let n = self.providers[self.tab].accounts().len();
        let quits = matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
            || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL));
        if quits && self.held.is_some() {
            self.mode = Mode::ConfirmQuit;
            return Ok(Action::None);
        }
        if quits {
            return Ok(Action::Quit);
        }
        match key.code {
            KeyCode::Char('x') => {
                if let Some(held) = self.held.take() {
                    self.status = format!("{} dropped", held.change.words());
                }
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
                if self.providers[self.tab].active() == Some(sel) {
                    self.status = format!("'{name}' is already active");
                } else {
                    return Ok(self.propose(Change::Switch(name)));
                }
            }
            KeyCode::Char('s') => {
                let entries = self.picker_entries(self.tab);
                if entries.is_empty() {
                    self.status = "no metrics to sort by yet".into();
                } else {
                    let current = self.sort_path(self.tab);
                    let cursor = entries
                        .iter()
                        .position(|path| Some(path) == current.as_ref())
                        .unwrap_or(0);
                    self.mode = Mode::Metrics(cursor);
                }
            }
            KeyCode::Char('S') => match self.sort_path(self.tab) {
                Some(path) => {
                    let descending = self.sort_pref(self.tab).descending;
                    self.set_sort(path, !descending);
                }
                None => self.status = "no metrics to sort by yet".into(),
            },
            KeyCode::Char('[') => self.cycle_sort(-1),
            KeyCode::Char(']') => self.cycle_sort(1),
            KeyCode::Char('i') => {
                self.prefs.hide_info = !self.prefs.hide_info;
                self.status = if self.prefs.hide_info {
                    "info panel hidden".into()
                } else {
                    "info panel shown".into()
                };
                self.save_prefs();
            }
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
            KeyCode::Char('f') => match self.registry().and_then(|r| r.presets()) {
                Ok(presets) if presets.is_empty() => self.status = "no presets to pick from".into(),
                Ok(presets) => self.mode = Mode::Form(Form::from_preset(&presets)),
                Err(e) => self.status = format!("{e:#}"),
            },
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
        let name = self.path_label(self.tab, &path);
        let pref = SortPref {
            metric: Some(path),
            descending,
        };
        self.status = format!("{provider} accounts ordered by {name} {}", pref.arrow());
        self.prefs.sort.insert(provider, pref);
        self.save_prefs();
    }

    // Preferences written to disk, a failure replaces the status line
    fn save_prefs(&mut self) {
        if let Err(e) = self.prefs.save(&self.prefs_path) {
            self.status = format!("could not save preferences: {e:#}");
        }
    }

    // Label for any metric path, shared leading segments dropped when the path carries them
    fn path_label(&self, tab: usize, path: &[String]) -> String {
        let union = self.metric_union(tab);
        let skip = common_prefix(union.iter().map(|m| m.path.as_slice()));
        let shared = union
            .first()
            .is_some_and(|m| path.len() > skip && m.path[..skip] == path[..skip]);
        label(path, if shared { skip } else { 0 })
    }

    // Saved column choice or the default set, in the order the metrics were reported
    fn base_columns(&self, tab: usize) -> Vec<Vec<String>> {
        let union = self.metric_union(tab);
        let mut columns = match self.prefs.columns.get(self.providers[tab].name()) {
            Some(saved) => saved.clone(),
            None => default_columns(&union),
        };
        columns.sort_by_key(|c| {
            union
                .iter()
                .position(|m| m.path == *c)
                .unwrap_or(usize::MAX)
        });
        columns
    }

    // Metric paths the accounts table shows, the sort metric always among them
    fn columns(&self, tab: usize) -> Vec<Vec<String>> {
        let union = self.metric_union(tab);
        let mut columns = self.base_columns(tab);
        if let Some(sort) = self.sort_path(tab) {
            if !columns.contains(&sort) {
                columns.push(sort);
            }
        }
        columns.sort_by_key(|c| {
            union
                .iter()
                .position(|m| m.path == *c)
                .unwrap_or(usize::MAX)
        });
        columns
    }

    // Bounded numbers, other numbers, then the rest, then shown columns nothing reports any more
    fn picker_entries(&self, tab: usize) -> Vec<Vec<String>> {
        let mut union = self.metric_union(tab);
        union.sort_by_key(|m| match (is_gauge(m), m.value.is_number()) {
            (true, _) => 0,
            (false, true) => 1,
            (false, false) => 2,
        });
        let mut entries: Vec<Vec<String>> = union.iter().map(|m| m.path.clone()).collect();
        for path in self.columns(tab) {
            if !entries.contains(&path) {
                entries.push(path);
            }
        }
        entries
    }

    // Shows or hides one metric as an accounts column, the sort metric keeps its column
    fn toggle_column(&mut self, path: Vec<String>) {
        let tab = self.tab;
        let name = self.path_label(tab, &path);
        if self.sort_path(tab).as_ref() == Some(&path) {
            self.status = format!("{name} orders the accounts, so its column stays");
            return;
        }
        let mut columns = self.base_columns(tab);
        self.status = match columns.iter().position(|c| *c == path) {
            Some(i) => {
                columns.remove(i);
                format!("{name} column hidden")
            }
            None => {
                columns.push(path);
                format!("{name} column shown")
            }
        };
        self.prefs
            .columns
            .insert(self.providers[tab].name().to_string(), columns);
        self.save_prefs();
    }

    // Hands the sort to the neighbouring accounts column, wrapping at both ends
    fn cycle_sort(&mut self, delta: isize) {
        let columns = self.columns(self.tab);
        if columns.is_empty() {
            self.status = "no metrics to sort by yet".into();
            return;
        }
        let at = self
            .sort_path(self.tab)
            .and_then(|sort| columns.iter().position(|c| *c == sort))
            .unwrap_or(0) as isize;
        let next = (at + delta).rem_euclid(columns.len() as isize) as usize;
        let descending = self.sort_pref(self.tab).descending;
        self.set_sort(columns[next].clone(), descending);
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
        // Borders, one row per account and a header once there are metric columns
        let list_height = match self.providers[self.tab].accounts().len() as u16 {
            0 => 3,
            n => n + 2 + u16::from(!self.columns(self.tab).is_empty()),
        };
        let info = if self.prefs.hide_info {
            Vec::new()
        } else {
            self.providers[self.tab].info()
        };
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
            render_info(f, info_area, &info, self.lead(self.tab));
        }
        self.render_usage(f, usage_area);
        self.render_footer(f, footer_area);
        match &self.mode {
            Mode::AddMethod(selected) => self.render_menu(f, *selected),
            Mode::Form(form) => render_form(f, form),
            Mode::Metrics(cursor) => self.render_metric_picker(f, *cursor),
            Mode::Providers(_) | Mode::ConfirmRemove(_) => {
                self.render_providers(f, self.providers_cursor());
            }
            Mode::Sessions(preview) => self.render_sessions(f, preview),
            _ => {}
        }
    }

    // Each live session with when the change reaches it and what that does to its thinking
    fn render_sessions(&self, f: &mut Frame, preview: &Preview) {
        let rows = &preview.sessions;
        let widest = |text: fn(&Session) -> &str, min: usize, max: usize| {
            rows.iter()
                .map(|s| text(s).chars().count())
                .max()
                .unwrap_or(min)
                .clamp(min, max)
        };
        let wait_w = Wait::Close.label().chars().count();
        let name_w = widest(|s| &s.name, 4, 20);
        let state_w = widest(|s| &s.now.state, 4, 26);
        let dir_w = widest(|s| &s.dir, 1, 30);
        let effect_w = widest(|s| &s.effect, 1, 100);
        let wanted = (4 + wait_w + name_w + state_w + dir_w + 8)
            .max(effect_w + 8)
            .max(70);
        let width = (wanted as u16).min(f.area().width.saturating_sub(2));
        let inner_w = width.saturating_sub(4) as usize;

        let mut lines: Vec<Line> = Vec::new();
        let mut focus_end = 0;
        for (i, (session, wait)) in rows.iter().zip(&preview.waits).enumerate() {
            let focused = i == preview.cursor;
            let state_style = if session.now.mid_turn {
                Style::new().yellow()
            } else {
                Style::new().green()
            };
            lines.push(Line::from(vec![
                Span::styled(if focused { "▶ " } else { "  " }, Style::new().cyan()),
                Span::styled(
                    pad(wait.label(), wait_w),
                    if *wait == Wait::Now {
                        Style::new().dim()
                    } else {
                        Style::new().fg(BAR).bold()
                    },
                ),
                Span::raw("  "),
                Span::styled(
                    pad(&session.name, name_w),
                    if focused {
                        Style::new().bold()
                    } else {
                        Style::new()
                    },
                ),
                Span::raw("  "),
                Span::styled(pad(&session.now.state, state_w), state_style),
                Span::raw("  "),
                Span::styled(tail(&session.dir, dir_w), Style::new().dim()),
            ]));
            let mut words = Vec::new();
            for (n, clause) in session.effect.split(" · ").enumerate() {
                if n > 0 {
                    words.push(Word::separator());
                }
                words.extend(clause.split(' ').map(|w| Word::new(w, Style::new().dim())));
            }
            push_wrapped(&mut lines, words, 4, inner_w);
            if focused {
                focus_end = lines.len();
            }
        }
        if rows.is_empty() {
            lines.push(Line::styled(
                "no sessions are left on the live login",
                Style::new().dim(),
            ));
        }

        let until = preview.until();
        let outcome = if until.is_empty() {
            format!("enter {} now", preview.change.does())
        } else {
            format!("enter {} once {until}", preview.change.does())
        };
        let keys = "space when it moves · ↑/↓ session · enter confirm · esc cancel";
        let hint_h = 1 + wrapped_height(&outcome, inner_w) + wrapped_height(keys, inner_w);
        let rect = centered(f.area(), width, lines.len() as u16 + hint_h + 2);
        f.render_widget(Clear, rect);
        let count = match rows.len() {
            1 => "1 live session".to_string(),
            n => format!("{n} live sessions"),
        };
        let block = panel(format!(
            " {} {} · {count} ",
            self.providers[self.tab].name(),
            preview.change.words()
        ));
        let inner = block.inner(rect);
        f.render_widget(block, rect);
        let [list_area, hint_area] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(hint_h)]).areas(inner);
        let scroll = focus_end.saturating_sub(list_area.height as usize);
        f.render_widget(Paragraph::new(lines).scroll((scroll as u16, 0)), list_area);
        f.render_widget(
            Paragraph::new(vec![
                Line::raw(""),
                Line::styled(outcome, Style::new().cyan()),
                Line::styled(keys, Style::new().dim()),
            ])
            .wrap(Wrap { trim: true }),
            hint_area,
        );
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
                    "enter enable/disable · n new · f from preset · e edit · d remove · esc close",
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

    // Each metric with its sort arrow, column checkbox and the selected account's reading
    fn render_metric_picker(&self, f: &mut Frame, cursor: usize) {
        let tab = self.tab;
        let p = &self.providers[tab];
        let entries = self.picker_entries(tab);
        let columns = self.columns(tab);
        let current = self.sort_path(tab);
        let pref = self.sort_pref(tab);
        let account = p
            .accounts()
            .get(self.selected[tab])
            .map(|a| a.name.clone())
            .unwrap_or_default();
        let labels: Vec<String> = entries.iter().map(|e| self.path_label(tab, e)).collect();
        let label_w = labels
            .iter()
            .map(|l| l.chars().count())
            .max()
            .unwrap_or(6)
            .clamp(6, (f.area().width as usize / 2).max(6));
        let value_w = 22;
        let items: Vec<ListItem> = entries
            .iter()
            .zip(&labels)
            .map(|(path, l)| {
                let marker = if current.as_ref() == Some(path) {
                    format!("{} ", pref.arrow())
                } else {
                    "  ".to_string()
                };
                let shown = columns.contains(path);
                let value = self
                    .metric_of(tab, &account, &path.join("."))
                    .map(|m| m.value.brief())
                    .unwrap_or_else(|| "—".to_string());
                ListItem::new(Line::from(vec![
                    Span::styled(marker, Style::new().fg(BAR).bold()),
                    Span::styled(
                        if shown { "[x] " } else { "[ ] " },
                        if shown {
                            Style::new().fg(BAR)
                        } else {
                            Style::new().dim()
                        },
                    ),
                    Span::raw(pad_label(l, label_w)),
                    Span::raw("  "),
                    Span::styled(clip(&value, value_w), Style::new().dim()),
                ]))
            })
            .collect();
        let rect = centered(
            f.area(),
            (label_w + value_w + 16) as u16,
            items.len() as u16 + 5,
        );
        f.render_widget(Clear, rect);
        let block = panel(format!(" {} metrics ", p.name()));
        let inner = block.inner(rect);
        f.render_widget(block, rect);
        let [list_area, hint_area] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(2)]).areas(inner);
        let last = items.len().saturating_sub(1);
        let mut state = ListState::default().with_selected(Some(cursor.min(last)));
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
                    "enter sort, again flips · space column on/off · esc close",
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

    // Width of the first column all three panels share, so names and labels start and end together
    fn lead(&self, tab: usize) -> usize {
        let p = &self.providers[tab];
        let accounts = p.accounts();
        let names = column(&accounts, |a| a.name.chars().count(), 4, LEAD_MAX);
        let info = if self.prefs.hide_info {
            Vec::new()
        } else {
            p.info()
        };
        let info = info
            .iter()
            .map(|(l, _)| l.chars().count())
            .max()
            .unwrap_or(0);
        let skip = self.label_skip(tab);
        let labels = accounts
            .iter()
            .filter_map(|a| match self.usage.get(&(tab, a.name.clone())) {
                Some(UsageState::Ready(u)) => Some(label_need(&outline_of(&u.metrics, skip))),
                _ => None,
            })
            .max()
            .unwrap_or(0);
        names.max(info).max(labels).min(LEAD_MAX)
    }

    fn render_accounts(&self, f: &mut Frame, area: Rect) {
        let width = area.width.saturating_sub(6) as usize; // borders, padding, selection arrow
        let tab = self.tab;
        let p = &self.providers[tab];
        let rows = p.accounts();
        let name_w = self.lead(tab);
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
        let sort = self.sort_path(tab);
        let sorted_by = sort.as_ref().map_or(String::new(), |path| {
            format!(
                " · {} {}",
                self.sort_pref(tab).arrow(),
                self.path_label(tab, path)
            )
        });
        let mut title = format!(" {} accounts ({})", p.name(), rows.len());
        if rows.is_empty() {
            title.push_str(&sorted_by);
            title.push(' ');
            f.render_widget(
                Paragraph::new(Line::styled(
                    "no accounts yet - press 'a' to add one",
                    Style::new().dim(),
                ))
                .block(panel(title)),
                area,
            );
            return;
        }

        let paths = self.columns(tab);
        let views: Vec<ColumnView> = paths
            .iter()
            .zip(column_labels(&paths, |path| self.path_label(tab, path)))
            .map(|(path, name)| self.column_view(tab, path, name, &rows, sort.as_ref()))
            .collect();
        let pending = rows
            .iter()
            .any(|r| !matches!(self.usage_state(&r.name), Some(UsageState::Ready(_))));
        let sorted = views.iter().position(|v| v.sorted);
        let fit = Fit::choose(
            &views,
            sorted,
            name_w + 2 + 2 + plan_w,
            mail_w,
            pending,
            width,
        );
        if fit.shown.len() < views.len() {
            title.push_str(&format!(
                " · {} of {} columns",
                fit.shown.len(),
                views.len()
            ));
        }
        title.push_str(&sorted_by);
        title.push(' ');

        let mut widths = vec![name_w + 2];
        let mut header = vec![Line::raw("")];
        if fit.mail {
            widths.push(mail_w);
            header.push(Line::raw(""));
        }
        widths.push(plan_w);
        header.push(Line::raw(""));
        for (k, &c) in fit.shown.iter().enumerate() {
            let w = fit.width(&views[c], k);
            widths.push(w + fit.pad(k));
            header.push(Line::from(Span::styled(
                views[c].heading(w),
                if views[c].sorted {
                    Style::new().fg(BAR).bold()
                } else {
                    Style::new().dim()
                },
            )));
        }
        // A tab with no metric columns still needs room for fetch status
        if fit.shown.is_empty() {
            widths.push(STATUS_W);
            header.push(Line::raw(""));
        }

        let order = self.order(tab);
        let table_rows: Vec<Row> = order
            .iter()
            .map(|&i| {
                let r = &rows[i];
                let active = p.active() == Some(i);
                let mut cells = vec![Line::from(vec![
                    Span::styled(if active { "● " } else { "  " }, Style::new().green()),
                    Span::styled(
                        clip(&r.name, name_w),
                        if active {
                            Style::new().green().bold()
                        } else {
                            Style::new()
                        },
                    ),
                ])];
                if fit.mail {
                    cells.push(Line::from(Span::styled(
                        clip(r.email.as_deref().unwrap_or("-"), mail_w),
                        Style::new().dim(),
                    )));
                }
                cells.push(Line::from(Span::styled(
                    clip(r.plan.as_deref().unwrap_or(""), plan_w),
                    Style::new().dim(),
                )));
                match self.usage_state(&r.name) {
                    Some(UsageState::Ready(_)) => {
                        for (k, &c) in fit.shown.iter().enumerate() {
                            let view = &views[c];
                            let metric = self.metric_of(tab, &r.name, &view.id);
                            cells.push(view.cell(metric, &fit, fit.width(view, k)));
                        }
                    }
                    Some(UsageState::Error(_)) => {
                        cells.push(Line::from(Span::styled("unavailable", Style::new().red())));
                    }
                    None | Some(UsageState::Loading) => {
                        cells.push(Line::from(Span::styled("fetching…", Style::new().dim())));
                    }
                }
                Row::new(cells)
            })
            .collect();

        let highlighted = order
            .iter()
            .position(|&i| i == self.selected[tab])
            .unwrap_or(0);
        let mut state = TableState::default().with_selected(Some(highlighted));
        let mut table = Table::new(
            table_rows,
            widths.into_iter().map(|w| Constraint::Length(w as u16)),
        );
        if !views.is_empty() {
            table = table.header(Row::new(header));
        }
        f.render_stateful_widget(
            table
                .column_spacing(2)
                .block(panel(title))
                .highlight_symbol("▶ ")
                .row_highlight_style(Style::new().bold()),
            area,
            &mut state,
        );
    }

    // One metric column sized to the readings every account holds for it
    fn column_view(
        &self,
        tab: usize,
        path: &[String],
        name: String,
        accounts: &[Account],
        sort: Option<&Vec<String>>,
    ) -> ColumnView {
        let id = path.join(".");
        let sorted = sort.is_some_and(|s| s.as_slice() == path);
        let (mut value_w, mut until_w, mut text_w) = (0, 0, 1);
        for a in accounts {
            let Some(m) = self.metric_of(tab, &a.name, &id) else {
                continue;
            };
            if m.value.is_number() {
                value_w = value_w.max(m.value.text().chars().count());
                if let Some(until) = &m.until {
                    until_w = until_w.max(until_note(until).chars().count());
                }
            } else {
                text_w = text_w.max(m.value.brief().chars().count().min(TEXT_W));
            }
        }
        ColumnView {
            relative_max: self.relative_max(tab, &id),
            header: name,
            arrow: sorted.then(|| self.sort_pref(tab).arrow()),
            id,
            sorted,
            value_w,
            until_w,
            text_w,
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

    // Labels in one column and every bar and fact starting in the next, nesting shown by label indent
    fn usage_lines(&self, u: &Usage, width: usize) -> Vec<Line<'static>> {
        let tab = self.tab;
        let sort_id = self.sort_id(tab);
        let arrow = self.sort_pref(tab).arrow();
        let entries = outline_of(&u.metrics, self.label_skip(tab));

        let gauges: Vec<&Metric> = entries
            .iter()
            .filter_map(|e| match e {
                Entry::Gauge { metric, .. } => Some(*metric),
                _ => None,
            })
            .collect();
        let value_w = gauges
            .iter()
            .map(|m| m.value.text().chars().count())
            .max()
            .unwrap_or(3)
            .max(3);
        let ceiling = |m: &Metric| m.value.ceiling().map(|limit| format!(" of {limit}"));
        let ceiling_w = gauges
            .iter()
            .filter_map(|m| ceiling(m))
            .map(|c| c.chars().count())
            .max()
            .unwrap_or(0);
        // The countdown carries the sort arrow when the reset time itself orders the accounts
        let dim = Style::new().dim();
        let marked = Style::new().fg(BAR).bold();
        let note_of = |m: &Metric| {
            m.until.as_ref().map(|until| {
                let mut sibling = m.path[..m.path.len() - 1].to_vec();
                sibling.push(until.key.clone());
                if sort_id.as_deref() == Some(sibling.join(".").as_str()) {
                    (format!("{arrow} {}", reset_note(until)), marked)
                } else {
                    (reset_note(until), dim)
                }
            })
        };
        let note_w = gauges
            .iter()
            .filter_map(|m| note_of(m))
            .map(|(note, _)| note.chars().count())
            .max()
            .unwrap_or(0);
        // Gutter, label, bar and value with the gaps between them, the label yields first
        let around = LEAD + 2 + 2 + value_w + ceiling_w;
        let label_w = self
            .lead(tab)
            .max(label_need(&entries))
            .min((width / 2).max(6))
            .min(width.saturating_sub(around + 8))
            .max(6);
        let inline = note_w > 0 && width >= around + label_w + 8 + 2 + note_w;
        let bar_w = width
            .saturating_sub(around + label_w + if inline { 2 + note_w } else { 0 })
            .clamp(8, 40);
        let value_x = LEAD + label_w + 2;

        let is_sort = |m: &Metric| sort_id.as_deref() == Some(m.id.as_str());
        let gutter = |sorted: bool| {
            if sorted {
                Span::styled(format!("  {arrow} "), marked)
            } else {
                Span::raw(" ".repeat(LEAD))
            }
        };
        // Short facts share value column lines with each kept whole, long ones follow on their own
        let fact_lines = |items: &[(String, &Metric)]| {
            let long = |key: &str, m: &Metric| {
                key.chars().count() + m.value.text().chars().count() >= LONG_FACT
            };
            let (mut rows, mut words) = (Vec::new(), Vec::new());
            let short = items.iter().filter(|(key, m)| !long(key, m));
            for (key, metric) in short.chain(items.iter().filter(|(key, m)| long(key, m))) {
                let whole = !long(key, metric);
                if !whole && !words.is_empty() {
                    push_wrapped(&mut rows, std::mem::take(&mut words), value_x, width);
                }
                if !words.is_empty() {
                    words.push(Word::separator());
                }
                let (key, style) = if is_sort(metric) {
                    (format!("{arrow} {key}"), marked)
                } else {
                    (key.clone(), dim)
                };
                let first = words.len();
                words.push(Word::new(&key, style));
                let value = metric.value.text();
                words.extend(value.split_whitespace().map(|w| Word::new(w, Style::new())));
                if whole {
                    let last = words.len() - 1;
                    words[first..last].iter_mut().for_each(|w| w.glued = true);
                } else {
                    push_wrapped(&mut rows, std::mem::take(&mut words), value_x, width);
                }
            }
            push_wrapped(&mut rows, words, value_x, width);
            rows
        };
        // Swaps the indent of a value column line for a gutter and a label
        let labelled = |row: &mut Line<'static>, sorted: bool, label: &str, style: Style| {
            let head = [
                gutter(sorted),
                Span::styled(pad_label(label, label_w), style),
                Span::raw("  "),
            ];
            row.spans.splice(0..1, head);
        };
        let mut lines = Vec::new();
        for entry in &entries {
            match entry {
                Entry::Head {
                    indent,
                    name,
                    facts,
                } => {
                    let label = format!("{}{name}", " ".repeat(*indent));
                    let bare = Line::from(vec![
                        gutter(false),
                        Span::styled(
                            clip(&label, width.saturating_sub(LEAD)),
                            Style::new().bold(),
                        ),
                    ]);
                    // A heading wider than the label column keeps its line, its facts start below
                    let mut rows = fact_lines(facts);
                    match rows.first_mut() {
                        Some(first) if label.chars().count() <= label_w => {
                            labelled(first, false, &label, Style::new().bold());
                        }
                        _ => rows.insert(0, bare),
                    }
                    lines.extend(rows);
                }
                Entry::Gauge {
                    indent,
                    name,
                    metric,
                    facts,
                } => {
                    let label = format!("{}{name}", " ".repeat(*indent));
                    let mut spans = vec![
                        gutter(is_sort(metric)),
                        Span::raw(pad_label(&label, label_w)),
                        Span::raw("  "),
                    ];
                    spans.extend(meter(
                        metric.value.fill(0.0).unwrap_or(0.0),
                        bar_w,
                        bar_style(&metric.value),
                    ));
                    spans.push(Span::styled(
                        format!("  {:>value_w$}", metric.value.text()),
                        Style::new().bold(),
                    ));
                    spans.push(Span::styled(
                        format!("{:<ceiling_w$}", ceiling(metric).unwrap_or_default()),
                        dim,
                    ));
                    let note = note_of(metric);
                    if let Some((note, style)) = note.as_ref().filter(|_| inline) {
                        spans.push(Span::styled(format!("  {note}"), *style));
                    }
                    lines.push(Line::from(spans));
                    // A countdown too wide for the bar's line moves under the bar
                    if let Some((note, style)) = note.filter(|_| !inline) {
                        let words = note.split(' ').map(|w| Word::new(w, style)).collect();
                        push_wrapped(&mut lines, words, value_x, width);
                    }
                    lines.extend(fact_lines(facts));
                }
                Entry::Fact { key, metric } => {
                    let sorted = is_sort(metric);
                    let style = if sorted { marked } else { dim };
                    let value = metric.value.text();
                    let mut words: Vec<Word> = value
                        .split_whitespace()
                        .map(|w| Word::new(w, Style::new()))
                        .collect();
                    let mut rows = Vec::new();
                    // A key wider than the label column runs on into its value
                    if key.chars().count() > label_w {
                        words.insert(0, Word::new(key, style));
                        push_wrapped(&mut rows, words, LEAD, width);
                        rows[0].spans[0] = gutter(sorted);
                    } else {
                        push_wrapped(&mut rows, words, value_x, width);
                        if rows.is_empty() {
                            rows.push(Line::from(Span::raw(" ".repeat(value_x))));
                        }
                        labelled(&mut rows[0], sorted, key, style);
                    }
                    lines.extend(rows);
                }
            }
        }
        lines
    }

    fn help(&self) -> &'static str {
        if self.session_picker {
            "↑/↓ select · enter launch session · s metrics · S flip · [/] sort column · i info · r refresh · q/esc cancel"
        } else {
            "←/→ provider · ↑/↓ select · enter switch · s metrics · S flip · [/] sort column · i info · a add · e edit · d delete · p providers · r refresh · q quit"
        }
    }

    // A held change with what it waits on and the key that drops it
    fn held_line(&self) -> Option<String> {
        self.held.as_ref().map(|held| {
            format!(
                "{} {} held until {} · x drops it",
                held.provider,
                held.change.words(),
                held.until
            )
        })
    }

    // Held change, status line and help, each as tall as its text wraps
    fn footer_height(&self, width: u16) -> u16 {
        let usable = width.saturating_sub(4).max(1) as usize;
        let held = self
            .held_line()
            .map_or(0, |line| wrapped_height(&line, usable));
        held + 1 + wrapped_height(self.help(), usable)
    }

    fn render_footer(&self, f: &mut Frame, area: Rect) {
        let top_line = match (&self.mode, &self.held) {
            (Mode::ConfirmQuit, Some(held)) => Line::styled(
                format!("quit and drop the held {}? (y/N)", held.change.words()),
                Style::new().yellow().bold(),
            ),
            (Mode::ConfirmQuit, None) => Line::styled("quit? (y/N)", Style::new().yellow().bold()),
            (Mode::ConfirmDelete(name), _) => Line::styled(
                format!("delete '{name}' from accio? (y/N)"),
                Style::new().yellow().bold(),
            ),
            (Mode::ConfirmRemove(name), _) => Line::styled(
                format!("remove provider '{name}'? its profiles stay on disk (y/N)"),
                Style::new().yellow().bold(),
            ),
            _ => Line::styled(self.status.clone(), Style::new().cyan()),
        };
        let usable = area.width.saturating_sub(4).max(1) as usize;
        let held = self.held_line();
        let held_h = held.as_ref().map_or(0, |line| wrapped_height(line, usable));
        let [held_area, status_area, help_area] = Layout::vertical([
            Constraint::Length(held_h),
            Constraint::Length(1),
            Constraint::Min(1),
        ])
        .areas(area);
        let padded = || Block::new().padding(Padding::horizontal(2));
        f.render_widget(
            Paragraph::new(Line::styled(
                held.unwrap_or_default(),
                Style::new().yellow(),
            ))
            .wrap(Wrap { trim: true })
            .block(padded()),
            held_area,
        );
        f.render_widget(Paragraph::new(top_line).block(padded()), status_area);
        f.render_widget(
            Paragraph::new(Line::styled(self.help(), Style::new().dim()))
                .wrap(Wrap { trim: true })
                .block(padded()),
            help_area,
        );
    }
}

const STATUS_W: usize = 11;
const LEAD: usize = 4;
const COLUMN_PAD: usize = 2;
const LEAD_MAX: usize = 24;
const TEXT_W: usize = 20;
const HEADER_W: usize = 22;
const LONG_FACT: usize = 48;

// One metric column of the accounts table and the widths its readings need
struct ColumnView {
    id: String,
    header: String,
    arrow: Option<&'static str>,
    sorted: bool,
    value_w: usize,
    until_w: usize,
    text_w: usize,
    relative_max: f64,
}

impl ColumnView {
    // Width that holds every reading under one arrangement of bar and countdown
    fn width(&self, bar_w: usize, resets: bool) -> usize {
        let mut numeric = 0;
        if self.value_w > 0 {
            numeric = self.value_w + if bar_w > 0 { bar_w + 1 } else { 0 };
            if resets && self.until_w > 0 {
                numeric += 1 + self.until_w;
            }
        }
        numeric
            .max(self.text_w)
            .max((self.header.chars().count() + if self.sorted { 2 } else { 0 }).min(HEADER_W))
    }

    // Column name clipped to the width, the sort arrow always kept after it
    fn heading(&self, width: usize) -> String {
        match self.arrow {
            Some(arrow) => format!("{} {arrow}", clip(&self.header, width.saturating_sub(2))),
            None => clip(&self.header, width),
        }
    }

    // Bar, value and countdown for a number, clipped text for anything else
    fn cell(&self, metric: Option<&Metric>, fit: &Fit, width: usize) -> Line<'static> {
        // A missing number keeps its place under the numbers, clear of the bar
        let Some(m) = metric else {
            let bar = if self.value_w > 0 && fit.bar_w > 0 {
                fit.bar_w + 1
            } else {
                0
            };
            let dash = format!("{}{:>w$}", " ".repeat(bar), "—", w = self.value_w.max(1));
            return Line::styled(dash, Style::new().dim());
        };
        let Some(fill) = m.value.fill(self.relative_max) else {
            return Line::raw(clip(&m.value.brief(), width));
        };
        let mut spans = Vec::new();
        if fit.bar_w > 0 {
            spans.extend(meter(fill, fit.bar_w, bar_style(&m.value)));
            spans.push(Span::raw(" "));
        }
        spans.push(Span::styled(
            format!("{:>w$}", m.value.text(), w = self.value_w),
            if self.sorted {
                Style::new().bold()
            } else {
                Style::new()
            },
        ));
        if let Some(until) = m.until.as_ref().filter(|_| fit.resets) {
            spans.push(Span::styled(
                format!(" {}", until_note(until)),
                Style::new().dim(),
            ));
        }
        Line::from(spans)
    }
}

// How much of each metric cell the table width affords and which columns made it in
struct Fit {
    mail: bool,
    bar_w: usize,
    resets: bool,
    pending: bool,
    shown: Vec<usize>,
}

impl Fit {
    // First arrangement holding every column, else the leanest one shedding columns from the end
    fn choose(
        views: &[ColumnView],
        sorted: Option<usize>,
        identity: usize,
        mail_w: usize,
        pending: bool,
        avail: usize,
    ) -> Fit {
        const LADDER: [(bool, usize, bool); 5] = [
            (true, 8, true),
            (false, 8, true),
            (false, 4, true),
            (false, 0, true),
            (false, 0, false),
        ];
        let mut fit = Fit {
            mail: false,
            bar_w: 0,
            resets: false,
            pending,
            shown: (0..views.len()).collect(),
        };
        for (mail, bar_w, resets) in LADDER {
            fit = Fit {
                mail,
                bar_w,
                resets,
                ..fit
            };
            if fit.need(views, identity, mail_w) <= avail {
                return fit;
            }
        }
        while fit.shown.len() > 1 && fit.need(views, identity, mail_w) > avail {
            let last = fit.shown.len() - 1;
            let drop = fit
                .shown
                .iter()
                .rposition(|&c| Some(c) != sorted)
                .unwrap_or(last);
            fit.shown.remove(drop);
        }
        fit
    }

    // Table width this arrangement takes, spacing included
    fn need(&self, views: &[ColumnView], identity: usize, mail_w: usize) -> usize {
        let metrics: usize = self
            .shown
            .iter()
            .enumerate()
            .map(|(k, &c)| 2 + self.width(&views[c], k) + self.pad(k))
            .sum();
        identity
            + if self.mail { mail_w + 2 } else { 0 }
            + if self.shown.is_empty() {
                2 + STATUS_W
            } else {
                metrics
            }
    }

    // Column width in this arrangement, the first column also holds fetch status
    fn width(&self, view: &ColumnView, position: usize) -> usize {
        let floor = if self.pending && position == 0 {
            STATUS_W
        } else {
            0
        };
        view.width(self.bar_w, self.resets).max(floor)
    }

    // Blank columns that set a metric column apart from the one after it
    fn pad(&self, position: usize) -> usize {
        if position + 1 < self.shown.len() {
            COLUMN_PAD
        } else {
            0
        }
    }
}

// A number with a ceiling, the only kind a bar can honestly fill
fn is_gauge(m: &Metric) -> bool {
    matches!(&m.value, MetricValue::Number { scale, .. } if *scale != Scale::Relative)
}

// Numbers that reset or expire, else every number with a ceiling
fn default_columns(union: &[&Metric]) -> Vec<Vec<String>> {
    let pick = |keep: &dyn Fn(&Metric) -> bool| -> Vec<Vec<String>> {
        union
            .iter()
            .filter(|m| keep(m))
            .map(|m| m.path.clone())
            .collect()
    };
    let resetting = pick(&|m| m.value.is_number() && m.until.is_some());
    if resetting.is_empty() {
        pick(&is_gauge)
    } else {
        resetting
    }
}

// Shortest header per column, the group name alone unless another column shares it
fn column_labels(paths: &[Vec<String>], full: impl Fn(&[String]) -> String) -> Vec<String> {
    let group = |p: &[String]| match p.len() {
        0 => String::new(),
        1 => label(p, 0),
        n => label(&p[n - 2..n - 1], 0),
    };
    let pair = |p: &[String]| label(&p[p.len().saturating_sub(2)..], 0);
    let unique = |form: &dyn Fn(&[String]) -> String, i: usize| {
        let mine = form(&paths[i]);
        paths
            .iter()
            .enumerate()
            .all(|(j, p)| j == i || form(p) != mine)
    };
    (0..paths.len())
        .map(|i| {
            if unique(&group, i) {
                group(&paths[i])
            } else if unique(&pair, i) {
                pair(&paths[i])
            } else {
                full(&paths[i])
            }
        })
        .collect()
}

// Countdown for a table cell, a loop arrow for resets and a stop arrow for expiry
fn until_note(until: &Until) -> String {
    let glyph = if until.key.to_ascii_lowercase().contains("expir") {
        "⇥"
    } else {
        "↻"
    };
    format!("{glyph} {}", humanize_until(until.at))
}

// Countdown and wall clock time for the end of a counted period
fn reset_note(until: &Until) -> String {
    format!(
        "{} {} ({})",
        until_verb(&until.key),
        humanize_until(until.at),
        local_stamp(until.at)
    )
}

// Metrics arranged under the objects that held them, children in response order
struct Node<'a> {
    name: String,
    metric: Option<&'a Metric>,
    children: Vec<Node<'a>>,
}

impl<'a> Node<'a> {
    fn grow(metrics: &'a [Metric], skip: usize) -> Self {
        let mut root = Node {
            name: String::new(),
            metric: None,
            children: Vec::new(),
        };
        for m in metrics {
            let mut at = &mut root;
            for segment in &m.path[skip.min(m.path.len().saturating_sub(1))..] {
                let i = match at.children.iter().position(|c| c.name == *segment) {
                    Some(i) => i,
                    None => {
                        at.children.push(Node {
                            name: segment.clone(),
                            metric: None,
                            children: Vec::new(),
                        });
                        at.children.len() - 1
                    }
                };
                at = &mut at.children[i];
            }
            at.metric = Some(m);
        }
        root
    }

    fn words(&self) -> String {
        label(std::slice::from_ref(&self.name), 0)
    }
}

type Facts<'a> = Vec<(String, &'a Metric)>;

// One printed piece of the usage outline, facts ride with the heading or bar they describe
enum Entry<'a> {
    Head {
        indent: usize,
        name: String,
        facts: Facts<'a>,
    },
    Gauge {
        indent: usize,
        name: String,
        metric: &'a Metric,
        facts: Facts<'a>,
    },
    Fact {
        key: String,
        metric: &'a Metric,
    },
}

impl Entry<'_> {
    // Columns the label asks of the label column, only a bar's label can ask past the shared width
    fn label_width(&self) -> usize {
        match self {
            Entry::Head { facts, .. } if facts.is_empty() => 0,
            Entry::Gauge { indent, name, .. } => indent + name.chars().count(),
            Entry::Head { indent, name, .. } => (indent + name.chars().count()).min(LEAD_MAX),
            Entry::Fact { key, .. } => key.chars().count().min(LEAD_MAX),
        }
    }
}

// Widest label an outline puts in the label column
fn label_need(entries: &[Entry]) -> usize {
    entries.iter().map(Entry::label_width).max().unwrap_or(0)
}

fn outline_of(metrics: &[Metric], skip: usize) -> Vec<Entry<'_>> {
    let mut entries = Vec::new();
    outline(&Node::grow(metrics, skip), 0, true, false, &mut entries);
    entries
}

// Lays a subtree out as headed groups, a group's only bounded number shares the heading line
fn outline<'a>(node: &Node<'a>, indent: usize, root: bool, sole: bool, out: &mut Vec<Entry<'a>>) {
    let gauges: Vec<(&Node<'a>, &'a Metric)> = node
        .children
        .iter()
        .filter_map(|c| c.metric.filter(|m| is_gauge(m)).map(|m| (c, m)))
        .collect();
    let merged = match gauges.as_slice() {
        [(child, metric)] if !root => Some((*child, *metric)),
        _ => None,
    };
    // A reset time already printed beside its bar is not repeated as a fact
    let beside_bar = |child: &Node, m: &Metric| {
        matches!(&m.value, MetricValue::Time(t) if gauges.iter().any(|(_, g)| {
            g.until.as_ref().is_some_and(|u| u.key == child.name && u.at == *t)
        }))
    };
    let (mut facts, mut bars, mut groups) = (Vec::new(), Vec::new(), Vec::new());
    for child in &node.children {
        let printed = merged.is_some_and(|(c, _)| std::ptr::eq(c, child));
        if let Some(m) = child.metric.filter(|m| !printed && !beside_bar(child, m)) {
            if is_gauge(m) {
                bars.push((child.words(), m));
            } else {
                facts.push((child.words(), m));
            }
        }
        if child.children.is_empty() {
            continue;
        }
        match lone_fact(child) {
            Some((key, m)) => facts.push((format!("{} {key}", child.words()), m)),
            None => groups.push(child),
        }
    }
    // An only subgroup holding nothing but further groups prints no heading and adds no indent
    let through = sole && merged.is_none() && facts.is_empty() && bars.is_empty();
    let inner = if root || through { indent } else { indent + 2 };
    let loose = if root {
        std::mem::take(&mut facts)
    } else {
        Vec::new()
    };
    if !root && !through {
        out.push(match merged {
            Some((_, metric)) => Entry::Gauge {
                indent,
                name: node.words(),
                metric,
                facts,
            },
            None => Entry::Head {
                indent,
                name: node.words(),
                facts,
            },
        });
    }
    out.extend(bars.into_iter().map(|(name, metric)| Entry::Gauge {
        indent: inner,
        name,
        metric,
        facts: Vec::new(),
    }));
    let sole = !root && groups.len() == 1;
    for group in groups {
        outline(group, inner, false, sole, out);
    }
    // Loose top level facts close the outline, one to a row
    out.extend(
        loose
            .into_iter()
            .map(|(key, metric)| Entry::Fact { key, metric }),
    );
}

// The single unbounded leaf beneath a group, with the keys leading to it spaced out
fn lone_fact<'a>(node: &Node<'a>) -> Option<(String, &'a Metric)> {
    let [only] = node.children.as_slice() else {
        return None;
    };
    match (only.metric, only.children.is_empty()) {
        (Some(m), true) if node.metric.is_none() && !is_gauge(m) => Some((only.words(), m)),
        (None, false) if node.metric.is_none() => {
            lone_fact(only).map(|(key, m)| (format!("{} {key}", only.words()), m))
        }
        _ => None,
    }
}

// A styled run of text kept whole when it fits a line, a separator never starts or ends one
struct Word {
    text: String,
    style: Style,
    separator: bool,
    // Stays on one line with the word after it whenever the whole run fits a line
    glued: bool,
}

impl Word {
    fn new(text: &str, style: Style) -> Self {
        Word {
            text: text.to_string(),
            style,
            separator: false,
            glued: false,
        }
    }

    fn separator() -> Self {
        Word {
            text: "·".to_string(),
            style: Style::new().dark_gray(),
            separator: true,
            glued: true,
        }
    }
}

// Words packed into lines behind an indent, wrapped at the width with overlong words split
fn push_wrapped(lines: &mut Vec<Line<'static>>, words: Vec<Word>, indent: usize, width: usize) {
    let room = width.saturating_sub(indent).max(1);
    let mut rows: Vec<Vec<Span<'static>>> = vec![Vec::new()];
    let mut used = 0;
    for (i, word) in words.iter().enumerate() {
        let mut rest: Vec<char> = word.text.chars().collect();
        if rest.is_empty() || (word.separator && used == 0) {
            continue;
        }
        // A glued run moves to the next line together, one too long for a line flows word by word
        let mut need = rest.len();
        let mut end = i;
        while words[end].glued && end + 1 < words.len() {
            end += 1;
            need += 1 + words[end].text.chars().count();
        }
        if need > room {
            need = rest.len();
            if word.separator {
                need += 1 + words
                    .get(i + 1)
                    .map_or(0, |next| next.text.chars().count())
                    .min(room);
            }
        }
        if used > 0 && used + 1 + need > room {
            rows.push(Vec::new());
            used = 0;
            if word.separator {
                continue;
            }
        }
        while used == 0 && rest.len() > room {
            let head: String = rest.drain(..room).collect();
            rows.last_mut()
                .unwrap()
                .push(Span::styled(head, word.style));
            rows.push(Vec::new());
        }
        let row = rows.last_mut().unwrap();
        if used > 0 {
            row.push(Span::raw(" "));
            used += 1;
        }
        used += rest.len();
        row.push(Span::styled(
            rest.into_iter().collect::<String>(),
            word.style,
        ));
    }
    for row in rows.into_iter().filter(|row| !row.is_empty()) {
        let mut spans = vec![Span::raw(" ".repeat(indent))];
        spans.extend(row);
        lines.push(Line::from(spans));
    }
}

// Lines a text takes once wrapped at word breaks
fn wrapped_height(text: &str, usable: usize) -> u16 {
    let (mut lines, mut used) = (1, 0);
    for word in text.split(' ') {
        let len = word.chars().count();
        if used > 0 && used + 1 + len > usable {
            lines += 1;
            used = 0;
        }
        used += len + usize::from(used > 0);
    }
    lines
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
            if fl.is_choice() {
                let (_, detail) = &fl.options[fl.chosen()];
                let room = value_w.saturating_sub(fl.value.chars().count() + 6);
                spans.push(Span::styled("< ".to_string(), Style::new().dim()));
                spans.push(Span::raw(fl.value.clone()));
                spans.push(Span::styled(" >  ".to_string(), Style::new().dim()));
                spans.push(Span::styled(clip(detail, room), Style::new().dim()));
            } else if fl.value.is_empty() {
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
            if focused && !fl.is_choice() {
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
    let keys = if form.fields.get(form.focus).is_some_and(Field::is_choice) {
        "←/→ pick · ↑/↓ field · enter next, saves on last · esc cancel"
    } else {
        "↑/↓ field · enter next, saves on last · esc cancel"
    };
    f.render_widget(
        Paragraph::new(vec![Line::raw(""), Line::styled(keys, Style::new().dim())]),
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

// Where the tab's files live and what it holds, on the same two columns as the other panels
fn render_info(f: &mut Frame, area: Rect, info: &[(String, String)], lead: usize) {
    let width = area.width.saturating_sub(4) as usize;
    let value_w = width.saturating_sub(LEAD + lead + 2).max(8);
    let lines: Vec<Line> = info
        .iter()
        .map(|(l, v)| {
            Line::from(vec![
                Span::raw(" ".repeat(LEAD)),
                Span::styled(pad(l, lead), Style::new().dim()),
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
    let full = (eighths / 8).min(width);
    let mut spans = vec![Span::styled("█".repeat(full), style)];
    let mut filled = full;
    // The partial cell sits on the track colour so the bar has no gap at its tip
    if full < width && !eighths.is_multiple_of(8) {
        spans.push(Span::styled(
            PARTIAL[eighths % 8].to_string(),
            style.bg(Color::DarkGray),
        ));
        filled += 1;
    }
    spans.push(Span::styled(
        "█".repeat(width - filled),
        Style::new().dark_gray(),
    ));
    spans
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
    use accio_provider::{parse_usage, SessionState};

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
        let base = std::env::temp_dir().join(format!("accio-tui-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let d = base.join("providers");
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
    fn plain_form_names_no_preset_and_preset_form_cycles_choices() {
        let dir = scratch_dir("presets");
        let registry = Registry::load_at(dir.clone()).unwrap();
        let mut app = App::new(
            fake_build(&registry).unwrap(),
            scratch("presets"),
            dir.clone(),
        );
        app.build = fake_build;

        press(&mut app, KeyCode::Char('p'));
        assert!(screen(&app, 100, 30).contains("f from preset"));
        press(&mut app, KeyCode::Char('n'));
        let shown = screen(&app, 100, 30);
        assert!(shown.contains("new provider"));
        assert!(!shown.contains("preset"));
        assert!(!shown.contains("bundled"));
        press(&mut app, KeyCode::Esc);

        let presets = registry.presets_dir().to_path_buf();
        fs::create_dir_all(&presets).unwrap();
        fs::write(
            presets.join("zed.json"),
            r#"{"command": ["zed", "--headless"], "home": "~/.zed"}"#,
        )
        .unwrap();
        press(&mut app, KeyCode::Char('p'));
        press(&mut app, KeyCode::Char('f'));
        let picked = |app: &App| match &app.mode {
            Mode::Form(f) => f.fields[0].value.clone(),
            _ => panic!("preset form expected"),
        };
        assert_eq!(picked(&app), "pi");
        assert!(screen(&app, 100, 30).contains("bundled"));
        press(&mut app, KeyCode::Right);
        assert_eq!(picked(&app), "zed");
        let shown = screen(&app, 100, 30);
        assert!(shown.contains("< zed >"));
        assert!(!shown.contains("bundled"));
        press(&mut app, KeyCode::Right);
        assert_eq!(picked(&app), "pi");
        press(&mut app, KeyCode::Left);
        assert_eq!(picked(&app), "zed");
        press(&mut app, KeyCode::Char('p'));
        assert_eq!(picked(&app), "pi");
        press(&mut app, KeyCode::Char('z'));
        assert_eq!(picked(&app), "zed");
        press(&mut app, KeyCode::Backspace);
        assert_eq!(picked(&app), "zed");

        press(&mut app, KeyCode::Enter);
        type_text(&mut app, "zed-work");
        press(&mut app, KeyCode::Enter);
        assert!(
            app.status.starts_with("defined 'zed-work'"),
            "{}",
            app.status
        );
        assert!(dir.join("zed-work.json").exists());
        let Mode::Form(form) = &app.mode else {
            panic!("definition form expected")
        };
        assert!(matches!(&form.action, FormAction::Definition { name } if name == "zed-work"));
        assert_eq!(form.fields[0].value, "zed --headless");
        assert_eq!(form.fields[1].value, "~/.zed");
        assert_eq!(app.providers[app.tab].name(), "zed-work");
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
        press(&mut app, KeyCode::Char('f'));
        let Mode::Form(form) = &app.mode else {
            panic!("preset form expected")
        };
        assert!(matches!(form.action, FormAction::FromPreset));
        assert_eq!(form.fields[0].value, "pi");
        let shown = screen(&app, 100, 30);
        assert!(shown.contains("< pi >"));
        assert!(shown.contains("bundled"));
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
        assert!(matches!(app.mode, Mode::Metrics(0)));
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
        assert!(shared.contains("│   ↑ session "));
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
        let line = |needle: &str| {
            wide.lines()
                .find(|l| l.contains(needle))
                .unwrap_or_else(|| panic!("missing {needle:?} in\n{wide}"))
                .to_string()
        };
        // Bars and facts start in one column however deep their group sits
        let at = |needle: &str| {
            let found = line(needle);
            found[..found.find(needle).unwrap()].chars().count()
        };
        let bar_at = |needle: &str| {
            let found = line(needle);
            found[..found.find('█').unwrap()].chars().count()
        };
        let value_x = bar_at("│   ↑ five hour");
        for label in ["│     seven day", "│       session", "│     extra usage"] {
            assert_eq!(bar_at(label), value_x, "{label}");
        }
        for value in [
            "severity normal",
            "severity warning · scope model display name Fable",
            "monthly limit 50 · is enabled yes",
            "4,521",
            "max",
        ] {
            assert_eq!(at(value), value_x, "{value}");
        }
        assert!(line("│   ↑ five hour").contains("60%"));
        assert!(line("│   ↑ five hour").contains("resets in 2h 59m ("));
        assert!(line("│     seven day").contains("42.5%"));
        assert!(line("│       weekly scoped").contains("78%"));
        assert!(line("│     extra usage").contains("12.5 of 50"));
        assert!(line("│     rows").contains("4,521"));
        assert!(line("│     organization tier").contains("max"));
        assert!(wide.contains("│     limits"));
        assert!(!wide.contains("percent ") && !wide.contains("· utilization  "));
        assert!(!wide.contains("12.50"), "trailing zeros trimmed");
        assert!(
            !wide.contains("resets at"),
            "a reset beside its bar is not listed again"
        );
        assert!(!wide.contains("details"));

        press(&mut app, KeyCode::Char('s'));
        let picker = screen(&app, 110, 32);
        println!("{picker}");
        assert!(picker.contains("fake metrics"));
        assert!(picker.contains("▶ ↑ [x] five hour · utilization"));
        assert!(picker.contains("[ ] rows"));
        press(&mut app, KeyCode::Esc);

        let narrow = screen(&app, 70, 18);
        println!("{narrow}");
        assert!(narrow.contains("pgup/pgdn"));
        assert!(narrow.lines().all(|l| l.chars().count() <= 70));
        assert!(narrow.contains("1-"));
        let reset = narrow
            .lines()
            .position(|l| l.contains("resets in 2h 59m ("))
            .expect("the countdown moves under its bar");
        assert!(narrow
            .lines()
            .nth(reset - 1)
            .unwrap()
            .contains("↑ five hour"));
        assert!(!narrow.contains("organization tier"));
        press(&mut app, KeyCode::PageDown);
        press(&mut app, KeyCode::PageDown);
        let scrolled = screen(&app, 70, 18);
        println!("{scrolled}");
        assert!(scrolled.contains("organization tier"));
        assert!(!scrolled.contains("1-"));

        // A reset time shown beside its bar takes the arrow there when it orders the accounts
        app.set_sort(vec!["five_hour".into(), "resets_at".into()], false);
        let by_reset = screen(&app, 110, 32);
        println!("{by_reset}");
        let marked = by_reset
            .lines()
            .find(|l| l.contains("↑ resets in 2h 59m ("))
            .expect("the countdown carries the arrow");
        assert!(marked.contains("│     five hour") && marked.contains("60%"));
        assert!(by_reset.contains("fake accounts (2) · ↑ five hour · resets at"));
        let table: Vec<&str> = by_reset.lines().skip(2).take(2).collect();
        assert!(table[0].contains("five hour · utilizati…") && table[0].contains("… ↑"));
        assert!(table[1].contains("↻ 2h 59m") && table[1].contains("in 2h 59m"));

        // A fact can order the accounts too and carries the arrow in its line
        app.set_sort(vec!["rows".into()], false);
        let by_fact = screen(&app, 110, 32);
        println!("{by_fact}");
        let by_rows = by_fact.lines().find(|l| l.contains("│   ↑ rows")).unwrap();
        assert!(by_rows.contains("4,521"));
        assert!(by_fact.contains("rows ↑"));
        let _ = fs::remove_file(&path);
    }

    fn quota(session: u32, weekly: u32, resets_at: i64) -> Value {
        serde_json::json!({
            "limits": [
                {"kind": "session", "percent": session, "resets_at": resets_at},
                {"kind": "weekly", "percent": weekly, "resets_at": resets_at}
            ],
            "spend": {"percent": 3, "currency": "USD"}
        })
    }

    #[test]
    fn account_columns_follow_the_saved_choice_and_the_sort_walks_them() {
        let path = scratch("columns");
        let _ = fs::remove_file(&path);
        let mut app = App::new(
            vec![Box::new(Fake(vec!["nick", "work", "slow", "down"]))],
            path.clone(),
            no_registry(),
        );
        let soon = chrono_now() + 3 * 3600;
        app.usage.insert(
            (0, "nick".into()),
            UsageState::Ready(parse_usage(&quota(60, 10, soon))),
        );
        app.usage.insert(
            (0, "work".into()),
            UsageState::Ready(parse_usage(&quota(5, 90, soon))),
        );
        app.usage.insert((0, "slow".into()), UsageState::Loading);
        app.usage
            .insert((0, "down".into()), UsageState::Error("503".into()));
        let path_of = |id: &str| id.split('.').map(str::to_string).collect::<Vec<_>>();
        assert_eq!(
            app.columns(0),
            vec![
                path_of("limits.session.percent"),
                path_of("limits.weekly.percent")
            ]
        );

        let shown = screen(&app, 110, 24);
        println!("{shown}");
        let rows: Vec<&str> = shown.lines().collect();
        let header = rows[2];
        assert!(header.contains("session ↑") && header.contains("weekly"));
        assert!(!header.contains("spend") && !header.contains("percent"));
        let work = rows.iter().find(|l| l.contains("work")).unwrap();
        assert!(work.contains("5%") && work.contains("90%") && work.contains("↻ 2h 59m"));
        // Every reading sits under its header
        let session_x = header[..header.find("session ↑").unwrap()].chars().count();
        assert_eq!(
            work.chars().position(|c| c == '█' || c == '▍'),
            Some(session_x)
        );
        assert!(rows
            .iter()
            .any(|l| l.contains("slow") && l.contains("fetching…")));
        assert!(rows
            .iter()
            .any(|l| l.contains("down") && l.contains("unavailable")));
        assert!(rows.iter().any(|l| l.contains("● nick")));

        assert_eq!(
            app.picker_entries(0),
            [
                "limits.session.percent",
                "limits.weekly.percent",
                "spend.percent",
                "limits.session.resets_at",
                "limits.weekly.resets_at",
                "spend.currency"
            ]
            .map(path_of)
        );

        // Space in the picker adds a column, the sort metric keeps its own
        press(&mut app, KeyCode::Char('s'));
        assert!(matches!(app.mode, Mode::Metrics(0)));
        press(&mut app, KeyCode::Char(' '));
        assert!(app
            .status
            .ends_with("orders the accounts, so its column stays"));
        assert_eq!(app.columns(0).len(), 2);
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Char(' '));
        assert_eq!(app.status, "spend · percent column shown");
        assert!(matches!(app.mode, Mode::Metrics(2)));
        let picker = screen(&app, 110, 24);
        assert!(picker.contains("▶   [x] spend · percent"));
        assert!(picker.contains("[ ] spend · currency"));
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.columns(0).len(), 3);
        assert!(screen(&app, 110, 24)
            .lines()
            .nth(2)
            .unwrap()
            .contains("spend"));
        assert_eq!(
            Prefs::load(&path).columns["fake"],
            vec![
                path_of("limits.session.percent"),
                path_of("limits.weekly.percent"),
                path_of("spend.percent")
            ]
        );

        // The sort steps across the shown columns and wraps
        press(&mut app, KeyCode::Char(']'));
        assert_eq!(app.sort_id(0).as_deref(), Some("limits.weekly.percent"));
        assert_eq!(app.order(0)[..2], [0, 1]);
        assert!(screen(&app, 110, 24)
            .lines()
            .nth(2)
            .unwrap()
            .contains("weekly ↑"));
        press(&mut app, KeyCode::Char(']'));
        press(&mut app, KeyCode::Char(']'));
        assert_eq!(app.sort_id(0).as_deref(), Some("limits.session.percent"));
        press(&mut app, KeyCode::Char('['));
        assert_eq!(app.sort_id(0).as_deref(), Some("spend.percent"));

        // Hiding a column sticks, a column nothing reports any more can still be hidden
        press(&mut app, KeyCode::Char('s'));
        assert!(matches!(app.mode, Mode::Metrics(2)));
        press(&mut app, KeyCode::Up);
        press(&mut app, KeyCode::Char(' '));
        assert_eq!(app.status, "limits · weekly · percent column hidden");
        press(&mut app, KeyCode::Esc);
        assert_eq!(
            app.columns(0),
            vec![path_of("limits.session.percent"), path_of("spend.percent")]
        );
        app.prefs
            .columns
            .get_mut("fake")
            .unwrap()
            .push(path_of("gone.percent"));
        let entries = app.picker_entries(0);
        assert_eq!(entries.last(), Some(&path_of("gone.percent")));
        assert!(screen(&app, 110, 24)
            .lines()
            .nth(2)
            .unwrap()
            .contains("gone"));
        app.mode = Mode::Metrics(entries.len() - 1);
        press(&mut app, KeyCode::Char(' '));
        assert_eq!(app.status, "gone · percent column hidden");
        assert!(!app.picker_entries(0).contains(&path_of("gone.percent")));
        assert!(matches!(app.mode, Mode::Metrics(_)));
        screen(&app, 110, 24);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn metric_cells_give_up_detail_before_columns_as_the_table_narrows() {
        let mut app = App::new(
            vec![Box::new(Fake(vec!["nick", "work"]))],
            scratch("fit"),
            no_registry(),
        );
        let soon = chrono_now() + 3 * 3600;
        let limits: Vec<Value> = ["alpha", "bravo", "charlie", "delta", "echo", "foxtrot"]
            .iter()
            .map(|kind| serde_json::json!({"kind": kind, "percent": 40, "resets_at": soon}))
            .collect();
        for name in ["nick", "work"] {
            app.usage.insert(
                (0, name.into()),
                UsageState::Ready(parse_usage(&serde_json::json!({ "limits": limits }))),
            );
        }
        app.set_sort(
            ["limits", "foxtrot", "percent"].map(String::from).to_vec(),
            false,
        );
        let header = |width: u16| {
            let shown = screen(&app, width, 24);
            println!("{shown}");
            assert!(shown.lines().all(|l| l.chars().count() <= width as usize));
            let lines: Vec<String> = shown.lines().map(str::to_string).collect();
            (lines[1].clone(), lines[2].clone(), lines[3].clone())
        };
        let (title, head, row) = header(200);
        assert!(!title.contains("columns"));
        assert!(head.contains("alpha") && head.contains("foxtrot ↑"));
        assert!(row.contains("████") && row.contains("↻ 2h 59m") && row.contains('-'));
        let (_, _, row) = header(168);
        assert!(row.contains("████") && row.contains("↻ 2h 59m") && !row.contains('-'));
        let (_, _, row) = header(150);
        assert!(row.contains('█') && !row.contains("█████") && row.contains("↻ 2h 59m"));
        let (title, head, row) = header(120);
        assert!(!title.contains("columns"));
        assert!(head.contains("alpha") && head.contains("foxtrot ↑"));
        assert!(!row.contains('█') && row.contains("↻ 2h 59m"));
        let (title, _, row) = header(80);
        assert!(!title.contains("columns"));
        assert!(!row.contains('█') && !row.contains('↻'));
        assert_eq!(row.matches("40%").count(), 6);
        let (title, head, row) = header(70);
        assert!(title.contains("fake accounts (2) · 5 of 6 columns · ↑ foxtrot · percent"));
        assert!(head.contains("alpha") && head.contains("foxtrot ↑") && !head.contains("echo"));
        assert_eq!(row.matches("40%").count(), 5);
    }

    #[test]
    fn a_group_shows_short_facts_then_long_ones_then_its_nested_groups() {
        let mut app = App::new(
            vec![Box::new(Fake(vec!["nick"]))],
            scratch("outline"),
            no_registry(),
        );
        app.usage.insert(
            (0, "nick".into()),
            UsageState::Ready(parse_usage(&serde_json::json!({
                "spend": {
                    "used": {"amount_minor": 0, "currency": "USD", "exponent": 2},
                    "percent": 0,
                    "severity": "normal",
                    "disclaimer": "Usage credits cover you when you hit your plan limits.",
                    "can_toggle": false
                },
                "shares": [
                    {"limit_index": 1, "used_percent_of_weekly": 43, "allowance_percent_of_weekly": 50}
                ],
                "member_dashboard_available": false
            }))),
        );
        let shown = screen(&app, 112, 24);
        println!("{shown}");
        let row = |needle: &str| {
            shown
                .lines()
                .position(|l| l.contains(needle))
                .unwrap_or_else(|| panic!("missing {needle:?} in\n{shown}"))
        };
        let order = [
            "│   ↑ spend",
            "severity normal · can toggle no",
            "disclaimer Usage credits cover you when you hit your plan limits.",
            "│       used",
            "│     shares",
            "│       0",
            "│         used percent of weekly",
            "│         allowance percent of weekly",
            "│     member dashboard available",
        ];
        for pair in order.windows(2) {
            assert_eq!(
                row(pair[0]) + 1,
                row(pair[1]),
                "{} then {}",
                pair[0],
                pair[1]
            );
        }
        let line = |needle: &str| shown.lines().nth(row(needle)).unwrap();
        assert!(line("│       used").contains("amount minor 0 · currency USD · exponent 2"));
        assert!(line("│       0").contains("limit index 1"));
        assert!(line("│     member dashboard available")
            .trim_end_matches('│')
            .trim_end()
            .ends_with("no"));
        let bars = shown.lines().filter(|l| l.contains("████")).count();
        assert_eq!(
            bars, 4,
            "one table row and three bounded numbers, nothing unbounded"
        );
    }

    #[test]
    fn info_panel_hides_and_returns_and_the_choice_persists() {
        let path = scratch("info-toggle");
        let _ = fs::remove_file(&path);
        let mut app = App::new(
            vec![Box::new(Named("zed".into(), false))],
            path.clone(),
            no_registry(),
        );
        assert!(screen(&app, 80, 20).contains("┌ info "));
        press(&mut app, KeyCode::Char('i'));
        assert_eq!(app.status, "info panel hidden");
        assert!(!screen(&app, 80, 20).contains("┌ info "));
        assert!(Prefs::load(&path).hide_info);
        let reopened = App::new(
            vec![Box::new(Named("zed".into(), false))],
            path.clone(),
            no_registry(),
        );
        assert!(!screen(&reopened, 80, 20).contains("┌ info "));
        press(&mut app, KeyCode::Char('i'));
        assert_eq!(app.status, "info panel shown");
        assert!(screen(&app, 80, 20).contains("┌ info "));
        assert!(!Prefs::load(&path).hide_info);
        fs::remove_file(&path).unwrap();
    }

    #[test]
    fn facts_wrap_between_items_and_split_only_what_cannot_fit() {
        let text = |lines: &[Line]| -> Vec<String> {
            lines
                .iter()
                .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
                .collect()
        };
        let plain = Style::new();
        let mut words = Vec::new();
        for (n, item) in ["severity normal", "is active no", "group weekly"]
            .iter()
            .enumerate()
        {
            if n > 0 {
                words.push(Word::separator());
            }
            words.extend(item.split(' ').map(|w| Word::new(w, plain)));
        }
        let mut lines = Vec::new();
        push_wrapped(&mut lines, words, 4, 34);
        assert_eq!(
            text(&lines),
            ["    severity normal · is active no", "    group weekly"]
        );

        let mut lines = Vec::new();
        push_wrapped(
            &mut lines,
            vec![
                Word::new("url", plain),
                Word::new("abcdefghijklmnopqrstuvwxyz", plain),
                Word::separator(),
                Word::new("ok", plain),
            ],
            2,
            13,
        );
        assert_eq!(
            text(&lines),
            ["  url", "  abcdefghijk", "  lmnopqrstuv", "  wxyz · ok"]
        );
        let mut lines = Vec::new();
        push_wrapped(&mut lines, Vec::new(), 2, 12);
        assert!(lines.is_empty());
    }

    type Shared<T> = std::rc::Rc<std::cell::RefCell<T>>;

    // A provider with live sessions the test moves and a log of every change made to it
    struct Live {
        active: usize,
        states: Shared<Vec<SessionState>>,
        log: Shared<Vec<String>>,
    }

    impl Provider for Live {
        fn name(&self) -> &str {
            "live"
        }
        fn accounts(&self) -> Vec<Account> {
            ["a", "b"]
                .iter()
                .map(|n| Account {
                    name: n.to_string(),
                    email: None,
                    plan: None,
                })
                .collect()
        }
        fn active(&self) -> Option<usize> {
            Some(self.active)
        }
        fn activate(&mut self, idx: usize) -> Result<()> {
            self.active = idx;
            self.log.borrow_mut().push(format!("activate {idx}"));
            Ok(())
        }
        fn delete(&mut self, _: &str) -> Result<()> {
            panic!("nothing deletes here")
        }
        fn add(&mut self, _: Option<&str>) -> Result<String> {
            panic!("logins run outside the key handler")
        }
        fn configure(
            &mut self,
            name: Option<&str>,
            values: &BTreeMap<String, String>,
        ) -> Result<String> {
            let name = name.unwrap_or("new");
            self.log
                .borrow_mut()
                .push(format!("configure {name} {values:?}"));
            Ok(format!("updated '{name}'"))
        }
        fn values(&self, _: &str) -> BTreeMap<String, String> {
            BTreeMap::from([("KEY".to_string(), "v".to_string())])
        }
        fn fetches(&self) -> Vec<Fetch> {
            Vec::new()
        }
        fn sessions(&self, target: Option<usize>) -> Vec<Session> {
            let to = target.map_or("a fresh login".to_string(), |i| {
                self.accounts()[i].name.clone()
            });
            self.states
                .borrow()
                .iter()
                .map(|state| Session {
                    now: state.clone(),
                    name: format!("s{}", state.id),
                    dir: "~/code/app".to_string(),
                    effect: format!(
                        "2 thinking blocks from a unreadable as {to} · 1 thinking block from x"
                    ),
                })
                .collect()
        }
        fn session_states(&self) -> Vec<SessionState> {
            self.states.borrow().clone()
        }
    }

    fn state(id: &str, mid_turn: bool) -> SessionState {
        SessionState {
            id: id.to_string(),
            state: if mid_turn { "mid-turn" } else { "idle" }.to_string(),
            mid_turn,
        }
    }

    fn live(
        name: &str,
        states: Vec<SessionState>,
    ) -> (App, Shared<Vec<SessionState>>, Shared<Vec<String>>) {
        let states = std::rc::Rc::new(std::cell::RefCell::new(states));
        let log = Shared::default();
        let provider = Live {
            active: 0,
            states: states.clone(),
            log: log.clone(),
        };
        let app = App::new(vec![Box::new(provider)], scratch(name), no_registry());
        (app, states, log)
    }

    #[test]
    fn a_switch_lists_the_live_sessions_and_waits_on_the_ones_held() {
        let (mut app, states, log) = live("hold", vec![state("1", true), state("2", false)]);
        press(&mut app, KeyCode::Down);
        assert!(matches!(press(&mut app, KeyCode::Enter), Action::None));
        assert!(matches!(app.mode, Mode::Sessions(_)));
        assert!(log.borrow().is_empty());
        let shown = screen(&app, 100, 30);
        println!("{shown}");
        assert!(shown.contains("┌ live switch to 'b' · 2 live sessions "));
        assert!(shown.contains("▶ after its turn   s1    mid-turn  ~/code/app"));
        assert!(shown.contains("  moves now        s2    idle      ~/code/app"));
        assert!(
            shown.contains("2 thinking blocks from a unreadable as b · 1 thinking block from x")
        );
        assert!(shown.contains("enter switches to 'b' once s1 ends its turn"));

        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Char(' '));
        press(&mut app, KeyCode::Char(' '));
        let shown = screen(&app, 100, 30);
        assert!(shown.contains("▶ after it closes  s2    idle      ~/code/app"));
        assert!(shown.contains("enter switches to 'b' once s1 ends its turn, s2 closes"));
        assert!(matches!(press(&mut app, KeyCode::Enter), Action::None));
        assert!(matches!(app.mode, Mode::Normal));
        assert!(log.borrow().is_empty());
        let shown = screen(&app, 100, 30);
        println!("{shown}");
        assert!(shown
            .contains("live switch to 'b' held until s1 ends its turn, s2 closes · x drops it"));

        states.borrow_mut()[0].mid_turn = false;
        assert!(matches!(app.poll(), Action::None));
        assert!(screen(&app, 100, 30).contains("held until s2 closes · x drops it"));
        assert!(matches!(press(&mut app, KeyCode::Char('q')), Action::None));
        assert!(matches!(app.mode, Mode::ConfirmQuit));
        assert!(screen(&app, 100, 30).contains("quit and drop the held switch to 'b'? (y/N)"));
        assert!(matches!(press(&mut app, KeyCode::Char('n')), Action::None));
        assert!(app.held.is_some());
        press(&mut app, KeyCode::Esc);
        assert!(matches!(press(&mut app, KeyCode::Char('y')), Action::Quit));

        // A held session that starts another turn holds the switch again
        *states.borrow_mut() = vec![state("1", true), state("3", true)];
        assert!(matches!(app.poll(), Action::None));
        assert_eq!(app.held.as_ref().unwrap().until, "s1 ends its turn");
        assert!(log.borrow().is_empty());
        states.borrow_mut()[0].mid_turn = false;
        assert!(matches!(app.poll(), Action::None));
        assert_eq!(*log.borrow(), ["activate 1"]);
        assert!(app.held.is_none());
        assert_eq!(app.status, "switched to 'b'");
        assert_eq!(app.selected[0], 1);
        assert!(matches!(press(&mut app, KeyCode::Char('q')), Action::Quit));
    }

    #[test]
    fn a_switch_goes_straight_through_when_nothing_is_live_or_nothing_is_held() {
        let (mut app, states, log) = live("clear", Vec::new());
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Enter);
        assert!(matches!(app.mode, Mode::Normal));
        assert_eq!(*log.borrow(), ["activate 1"]);

        // Every session set to move now, the switch is made on confirm
        *states.borrow_mut() = vec![state("1", true)];
        press(&mut app, KeyCode::Up);
        press(&mut app, KeyCode::Enter);
        assert!(matches!(app.mode, Mode::Sessions(_)));
        press(&mut app, KeyCode::Char(' '));
        press(&mut app, KeyCode::Char(' '));
        assert!(screen(&app, 100, 30).contains("enter switches to 'a' now"));
        press(&mut app, KeyCode::Enter);
        assert_eq!(*log.borrow(), ["activate 1", "activate 0"]);
        assert!(app.held.is_none());

        // A turn that ends before the confirm leaves nothing to wait on
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Enter);
        states.borrow_mut()[0].mid_turn = false;
        press(&mut app, KeyCode::Enter);
        assert_eq!(log.borrow().len(), 3);

        // Cancelling the preview and dropping a hold both leave the login alone
        states.borrow_mut()[0].mid_turn = true;
        press(&mut app, KeyCode::Up);
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.status, "switch to 'a' cancelled");
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Enter);
        assert!(app.held.is_some());
        press(&mut app, KeyCode::Char('x'));
        assert_eq!(app.status, "switch to 'a' dropped");
        states.borrow_mut().clear();
        assert!(matches!(app.poll(), Action::None));
        assert_eq!(log.borrow().len(), 3);
    }

    #[test]
    fn an_open_preview_follows_sessions_as_they_start_close_and_change_state() {
        let (mut app, states, _log) = live("follow", vec![state("1", true), state("2", false)]);
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Char(' '));
        press(&mut app, KeyCode::Char(' '));
        *states.borrow_mut() = vec![state("0", true), state("1", false), state("2", true)];
        app.poll();
        let Mode::Sessions(preview) = &app.mode else {
            panic!("the preview stays open");
        };
        assert_eq!(preview.waits, [Wait::Turn, Wait::Turn, Wait::Close]);
        assert_eq!(preview.cursor, 2);
        assert!(!preview.sessions[1].now.mid_turn);
        assert_eq!(preview.until(), "s0 ends its turn, s2 closes");

        states.borrow_mut().clear();
        app.poll();
        let shown = screen(&app, 100, 30);
        assert!(shown.contains("no sessions are left on the live login"));
        assert!(shown.contains("enter switches to 'b' now"));
        press(&mut app, KeyCode::Char(' '));
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.status, "switched to 'b'");
    }

    #[test]
    fn a_login_and_an_edit_of_the_active_profile_wait_on_live_sessions_too() {
        let (mut app, states, log) = live("login", vec![state("1", true)]);
        press(&mut app, KeyCode::Char('a'));
        assert!(matches!(app.mode, Mode::Form(_)));
        type_text(&mut app, "fresh");
        assert!(matches!(press(&mut app, KeyCode::Enter), Action::None));
        let shown = screen(&app, 100, 30);
        assert!(shown.contains("┌ live new login · 1 live session "));
        assert!(shown.contains("unreadable as a fresh login"));
        assert!(shown.contains("enter starts the login once s1 ends its turn"));
        press(&mut app, KeyCode::Enter);
        assert!(screen(&app, 100, 30).contains("live new login held until s1 ends its turn"));
        assert!(matches!(app.poll(), Action::None));
        states.borrow_mut()[0].mid_turn = false;
        match app.poll() {
            Action::Add(0, Some(name)) => assert_eq!(name, "fresh"),
            _ => panic!("the login runs once the turn ends"),
        }
        assert!(app.held.is_none());

        // Saving the active profile is held the same way, another profile saves at once
        states.borrow_mut()[0].mid_turn = true;
        press(&mut app, KeyCode::Char('e'));
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Enter);
        assert!(screen(&app, 100, 30).contains("┌ live edit of 'a' · 1 live session "));
        press(&mut app, KeyCode::Enter);
        assert!(log.borrow().is_empty());
        states.borrow_mut().clear();
        app.poll();
        assert_eq!(*log.borrow(), [r#"configure a {"KEY": "v"}"#]);
        press(&mut app, KeyCode::Down);
        *states.borrow_mut() = vec![state("1", true)];
        press(&mut app, KeyCode::Char('e'));
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Enter);
        assert!(matches!(app.mode, Mode::Normal));
        assert_eq!(log.borrow().len(), 2);
    }

    // Accounts with emails, plans and an info panel, shaped like the claude tab
    struct Full;
    impl Provider for Full {
        fn name(&self) -> &str {
            "claude"
        }
        fn accounts(&self) -> Vec<Account> {
            [
                ("ghostnet-sh", "nick@ghostnet.sh", "max"),
                ("nixksphone", "nixksphone@gmail.com", "max"),
                ("slopblock-ai-last-one", "nick@slopblock.ai", "max"),
                ("192-168-1-27", "192.168.1.27", ""),
            ]
            .iter()
            .map(|(name, email, plan)| Account {
                name: name.to_string(),
                email: Some(email.to_string()),
                plan: (!plan.is_empty()).then(|| plan.to_string()),
            })
            .collect()
        }
        fn active(&self) -> Option<usize> {
            Some(1)
        }
        fn activate(&mut self, _: usize) -> Result<()> {
            panic!("nothing switches here")
        }
        fn delete(&mut self, _: &str) -> Result<()> {
            panic!("nothing deletes here")
        }
        fn add(&mut self, _: Option<&str>) -> Result<String> {
            panic!("nothing logs in here")
        }
        fn fetches(&self) -> Vec<Fetch> {
            Vec::new()
        }
        fn info(&self) -> Vec<(String, String)> {
            [
                ("profiles", "~/.config/accio/accounts/claude"),
                ("credentials", "~/.config/accio/accounts/claude/nixksphone"),
                ("login", "claude /login"),
            ]
            .iter()
            .map(|(l, v)| (l.to_string(), v.to_string()))
            .collect()
        }
    }

    fn claude_usage(week: u32, scoped: u32, resets_at: i64) -> Value {
        let limit = |percent: u32, active: bool| {
            serde_json::json!({"percent": percent, "resets_at": resets_at, "group": "weekly",
                "severity": "normal", "is_active": active})
        };
        let mut scoped = limit(scoped, true);
        scoped["scope"] = serde_json::json!({"model": {"display_name": "Fable"}});
        serde_json::json!({
            "seven_day": {"utilization": week, "resets_at": resets_at},
            "extra_usage": {"is_enabled": false, "user_disabled": false,
                "spend_limit_reached": false, "credits_ever_enabled": false},
            "limits": {"weekly_all": limit(week, false), "weekly_scoped": scoped},
            "spend": {"percent": 0, "severity": "normal", "enabled": false,
                "disclaimer": "Usage credits cover you when you hit your plan limits. [Learn more](https://support.claude.com/articles/12429409)",
                "can_purchase_credits": false, "can_toggle": false,
                "used": {"amount_minor": 0, "currency": "USD", "exponent": 2}},
            "seven_day_breakdown": {"window_started_at": chrono_now() - 86400,
                "rows": {"claude_code": {"percent": 100, "display_name": "Claude Code"},
                         "chat": {"percent": 0, "display_name": "Chats"}}},
        })
    }

    #[test]
    fn every_panel_keeps_its_labels_in_one_column_and_its_values_in_the_next() {
        let mut app = App::new(vec![Box::new(Full)], scratch("grid"), no_registry());
        let soon = chrono_now() + 5 * 86400 + 7200 + 30;
        let ready = |v: Value| UsageState::Ready(parse_usage(&v));
        app.usage
            .insert((0, "ghostnet-sh".into()), ready(claude_usage(0, 0, soon)));
        app.usage
            .insert((0, "nixksphone".into()), ready(claude_usage(1, 2, soon)));
        app.usage.insert(
            (0, "slopblock-ai-last-one".into()),
            ready(claude_usage(73, 100, soon)),
        );
        app.usage.insert(
            (0, "192-168-1-27".into()),
            ready(serde_json::json!({"base_url": "http://192.168.1.27:8080"})),
        );
        app.selected[0] = 1;
        let shown = screen(&app, 150, 44);
        println!("{shown}");
        let column_in = |text: &str, needle: &str| {
            let line = text
                .lines()
                .find(|l| l.contains(needle))
                .unwrap_or_else(|| panic!("missing {needle:?} in\n{text}"));
            line[..line.find(needle).unwrap()].chars().count()
        };
        let at = |needle: &str| column_in(&shown, needle);
        let usage = &shown[shown.find("┌ usage").unwrap()..];
        let bar_at = |text: &str, needle: &str| {
            let line = text.lines().find(|l| l.contains(needle)).unwrap();
            line.chars().position(|c| ('█'..='▏').contains(&c)).unwrap()
        };

        // Names, info labels and metric labels start together, nested labels two columns in
        let label_x = at("ghostnet-sh");
        for label in [
            "profiles",
            "credentials",
            "extra usage",
            "seven day breakdown",
        ] {
            assert_eq!(at(label), label_x, "{label}");
        }
        for nested in [
            "weekly all",
            "weekly scoped",
            "used ",
            "claude code",
            "chat ",
        ] {
            assert_eq!(column_in(usage, nested), label_x + 2, "{nested}");
        }
        assert!(shown.contains("slopblock-ai-last-one"));
        assert!(
            !usage.contains("rows"),
            "a heading with nothing of its own is not printed"
        );

        // Emails, info values, bars and facts start together in the next column
        let value_x = at("nick@ghostnet.sh");
        for value in [
            "~/.config/accio/accounts/claude ",
            "claude /login",
            "is enabled no · user disabled no · spend limit reached no · credits ever enabled no",
            "group weekly · severity normal · is active no",
            "severity normal · enabled no · can purchase credits no · can toggle no",
            "disclaimer Usage credits",
            "amount minor 0 · currency USD · exponent 2",
            "window started at",
            "display name Claude Code",
        ] {
            assert_eq!(at(value), value_x, "{value}");
        }
        for bar in [
            "↑ seven day",
            "weekly all",
            "weekly scoped",
            "│     spend",
            "claude code",
            "chat ",
        ] {
            assert_eq!(bar_at(usage, bar), value_x, "{bar}");
        }
        let row = |needle: &str| shown.lines().position(|l| l.contains(needle)).unwrap();
        assert!(row("can toggle no") < row("disclaimer Usage credits"));

        // Table headers sit over their bars and a missing reading sits under the numbers
        let table = &shown[..shown.find("┌ info").unwrap()];
        let header_x = column_in(table, "seven day ↑");
        assert_eq!(bar_at(table, "ghostnet-sh"), header_x);
        assert_eq!(bar_at(table, "slopblock-ai-last-one"), header_x);
        assert_eq!(
            column_in(table, "—"),
            column_in(table, "0% ") + 1,
            "the dash ends where the percentages end"
        );

        // A fact never leaves its value behind when the line runs out
        for width in [110, 84] {
            let narrow = screen(&app, width, 44);
            println!("{narrow}");
            assert!(narrow
                .lines()
                .all(|l| !l.trim_end_matches([' ', '│']).ends_with("reached")));
            assert!(narrow.contains("spend limit reached no"));
            assert_eq!(
                column_in(&narrow, "extra usage"),
                column_in(&narrow, "ghostnet-sh")
            );
        }

        app.selected[0] = 3;
        let facts = screen(&app, 150, 30);
        println!("{facts}");
        assert_eq!(column_in(&facts, "base url"), label_x);
        assert_eq!(column_in(&facts, "http://192.168.1.27:8080"), value_x);
    }

    fn chrono_now() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
    }
}
