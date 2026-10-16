//! custom providers - any harness described by a json definition instead of a crate

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use accio_provider::{
    canon_str, expand, files, sanitize_name, session, write_atomic, Backend, Job, Knob, Provider,
    Swap,
};
use anyhow::{bail, Context, Result};
use serde_json::{json, Map, Value};

pub const BUILTIN: &[&str] = &["claude", "codex", "grok", "gemini"];
const RESERVED: &[&str] = &["list", "add", "configure", "remove", "help", "provider"];
const FIELDS: &[&str] = &[
    "command", "home", "home_env", "copy", "unset", "env", "knobs", "files", "disabled",
];
const ENV_FILE: &str = ".accio.env";
const MERGE: &str = "merge:";

#[derive(Clone)]
pub struct Definition {
    pub name: String,
    pub path: PathBuf,
    pub builtin: bool,
    pub disabled: bool,
    pub command: Vec<String>,
    pub home: String,
    pub home_env: Option<String>,
    pub copy: Vec<String>,
    pub unset: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub knobs: Vec<Knob>,
    pub files: BTreeMap<String, Value>,
}

pub struct Registry {
    dir: PathBuf,
    pub definitions: Vec<Definition>,
}

pub fn provider(def: &Definition) -> Result<impl Provider> {
    Swap::load(Custom::new(def)?)
}

pub fn session_provider(def: &Definition) -> Result<impl Provider> {
    Swap::load_saved(Custom::new(def)?)
}

pub fn dir() -> Result<PathBuf> {
    Ok(dirs::config_dir()
        .context("cant determine config dir")?
        .join("accio")
        .join("providers"))
}

impl Registry {
    pub fn load() -> Result<Self> {
        Self::load_at(dir()?)
    }

    // Built ins in their fixed order, then customs by name
    pub fn load_at(dir: PathBuf) -> Result<Self> {
        let mut definitions: Vec<Definition> = BUILTIN
            .iter()
            .map(|name| Definition::builtin(name, &dir))
            .collect();
        let mut customs = Vec::new();
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Registry { dir, definitions })
            }
            Err(e) => return Err(e).with_context(|| format!("cant read {}", dir.display())),
        };
        for entry in entries {
            let path = entry?.path();
            if path.extension().is_none_or(|e| e != "json") {
                continue;
            }
            let Some(name) = path
                .file_stem()
                .and_then(|s| s.to_str())
                .map(str::to_string)
            else {
                continue;
            };
            let bytes = fs::read(&path).with_context(|| format!("cant read {}", path.display()))?;
            let value: Value = serde_json::from_slice(&bytes)
                .with_context(|| format!("invalid JSON in {}", path.display()))?;
            match definitions.iter_mut().find(|d| d.name == name) {
                Some(def) => def.disabled = builtin_disabled(&name, &path, &value)?,
                None => customs.push(Definition::parse(&name, &path, &value)?),
            }
        }
        customs.sort_by(|a, b| a.name.cmp(&b.name));
        definitions.extend(customs);
        Ok(Registry { dir, definitions })
    }

    pub fn get(&self, name: &str) -> Option<&Definition> {
        self.definitions.iter().find(|d| d.name == name)
    }

    pub fn enabled(&self) -> impl Iterator<Item = &Definition> {
        self.definitions.iter().filter(|d| !d.disabled)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(format!("{name}.json"))
    }

    // A preset when one exists for the name, a skeleton to edit otherwise
    pub fn add(&self, name: &str, command: Option<&str>) -> Result<PathBuf> {
        check_name(name)?;
        anyhow::ensure!(
            !BUILTIN.contains(&name),
            "'{name}' is built in - `accio provider enable {name}` or `disable` instead"
        );
        let path = self.path(name);
        anyhow::ensure!(
            !path.exists(),
            "'{name}' is already defined in {}",
            path.display()
        );
        let body = match preset(name) {
            Some(body) if command.is_none() => body.to_string(),
            _ => skeleton(name, command.unwrap_or(name)),
        };
        Definition::parse(name, &path, &serde_json::from_str(&body)?)?;
        fs::create_dir_all(&self.dir)
            .with_context(|| format!("cant create {}", self.dir.display()))?;
        write_atomic(&path, body.as_bytes())?;
        Ok(path)
    }

    pub fn remove(&self, name: &str) -> Result<()> {
        let def = self
            .get(name)
            .with_context(|| format!("no provider named '{name}' - see `accio provider list`"))?;
        anyhow::ensure!(
            !def.builtin,
            "'{name}' is built in - `accio provider disable {name}` instead"
        );
        fs::remove_file(&def.path).with_context(|| format!("cant remove {}", def.path.display()))
    }

    pub fn set_disabled(&self, name: &str, disabled: bool) -> Result<()> {
        self.get(name)
            .with_context(|| format!("no provider named '{name}' - see `accio provider list`"))?;
        let path = self.path(name);
        let mut value = session::read_object(&path)?;
        let obj = value.as_object_mut().unwrap();
        if disabled {
            obj.insert("disabled".into(), Value::Bool(true));
        } else {
            obj.remove("disabled");
        }
        if obj.is_empty() {
            if path.exists() {
                fs::remove_file(&path)
                    .with_context(|| format!("cant remove {}", path.display()))?;
            }
            return Ok(());
        }
        fs::create_dir_all(&self.dir)
            .with_context(|| format!("cant create {}", self.dir.display()))?;
        write_atomic(&path, serde_json::to_string_pretty(&value)?.as_bytes())
    }

    // The file as lines a form can edit, built ins have none
    pub fn draft(&self, name: &str) -> Result<Draft> {
        let def = self
            .get(name)
            .with_context(|| format!("no provider named '{name}' - see `accio provider list`"))?;
        anyhow::ensure!(
            !def.builtin,
            "'{name}' is built in - only enable and disable apply"
        );
        Draft::from_value(name, &session::read_object(&def.path)?)
    }

    // Validated and written whole, the disabled flag carried over
    pub fn save(&self, draft: &Draft) -> Result<PathBuf> {
        let name = draft.name.trim();
        check_name(name)?;
        anyhow::ensure!(
            !BUILTIN.contains(&name),
            "'{name}' is built in - `accio provider enable {name}` or `disable` instead"
        );
        let path = self.path(name);
        let mut value = draft.to_value()?;
        if session::read_object(&path)?.get("disabled") == Some(&Value::Bool(true)) {
            value["disabled"] = Value::Bool(true);
        }
        Definition::parse(name, &path, &value)?;
        fs::create_dir_all(&self.dir)
            .with_context(|| format!("cant create {}", self.dir.display()))?;
        let mut body = serde_json::to_string_pretty(&value)?;
        body.push('\n');
        write_atomic(&path, body.as_bytes())?;
        Ok(path)
    }
}

// A definition as single lines, lists comma joined, one row per env, knob and file
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Draft {
    pub name: String,
    pub command: String,
    pub home: String,
    pub home_env: String,
    pub copy: String,
    pub unset: String,
    pub env: Vec<String>,
    pub knobs: Vec<String>,
    pub files: Vec<String>,
}

impl Draft {
    fn from_value(name: &str, value: &Value) -> Result<Self> {
        let obj = value
            .as_object()
            .with_context(|| format!("expected an object for '{name}'"))?;
        let text = |field: &str| {
            obj.get(field)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        let items = |field: &str| -> Vec<&str> {
            obj.get(field)
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default()
        };
        let command = match obj.get("command") {
            Some(Value::Array(_)) => items("command").join(" "),
            _ => text("command"),
        };
        let env = obj
            .get("env")
            .and_then(Value::as_object)
            .map(|m| {
                m.iter()
                    .map(|(k, v)| format!("{k}={}", v.as_str().unwrap_or("")))
                    .collect()
            })
            .unwrap_or_default();
        let knobs = obj
            .get("knobs")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(knob_line).collect())
            .unwrap_or_default();
        let files = obj
            .get("files")
            .and_then(Value::as_object)
            .map(|m| m.iter().map(|(k, v)| format!("{k} {v}")).collect())
            .unwrap_or_default();
        Ok(Draft {
            name: name.to_string(),
            command,
            home: text("home"),
            home_env: text("home_env"),
            copy: items("copy").join(", "),
            unset: items("unset").join(", "),
            env,
            knobs,
            files,
        })
    }

    // Empty lines leave their field out
    fn to_value(&self) -> Result<Value> {
        let mut obj = Map::new();
        let command: Vec<&str> = self.command.split_whitespace().collect();
        match command.as_slice() {
            [] => {}
            [one] => {
                obj.insert("command".into(), json!(one));
            }
            many => {
                obj.insert("command".into(), json!(many));
            }
        }
        for (field, value) in [("home", &self.home), ("home_env", &self.home_env)] {
            if !value.trim().is_empty() {
                obj.insert(field.into(), json!(value.trim()));
            }
        }
        for (field, value) in [("copy", &self.copy), ("unset", &self.unset)] {
            let list: Vec<&str> = value
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect();
            if !list.is_empty() {
                obj.insert(field.into(), json!(list));
            }
        }
        let lines = |rows: &[String]| -> Vec<String> {
            rows.iter()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect()
        };
        let mut env = Map::new();
        for line in lines(&self.env) {
            let (k, v) = line
                .split_once('=')
                .with_context(|| format!("env line '{line}' must be KEY=VALUE"))?;
            env.insert(k.trim().to_string(), json!(v.trim()));
        }
        if !env.is_empty() {
            obj.insert("env".into(), Value::Object(env));
        }
        let knobs: Vec<Value> = lines(&self.knobs)
            .iter()
            .map(|l| parse_knob_line(l))
            .collect();
        if !knobs.is_empty() {
            obj.insert("knobs".into(), Value::Array(knobs));
        }
        let mut files = Map::new();
        for line in lines(&self.files) {
            let (key, template) = line.split_once(char::is_whitespace).with_context(|| {
                format!("file line '{line}' needs a json template after the path")
            })?;
            let template: Value = serde_json::from_str(template.trim())
                .with_context(|| format!("template for '{key}' must be json"))?;
            files.insert(key.to_string(), template);
        }
        if !files.is_empty() {
            obj.insert("files".into(), Value::Object(files));
        }
        Ok(Value::Object(obj))
    }
}

// Masked knobs lead with *, the hint follows the name
fn knob_line(knob: &Value) -> Option<String> {
    let obj = knob.as_object()?;
    let name = obj.get("name")?.as_str()?;
    let hint = obj.get("hint").and_then(Value::as_str).unwrap_or("");
    let secret = obj
        .get("secret")
        .and_then(Value::as_bool)
        .unwrap_or_else(|| looks_secret(name));
    let star = if secret { "*" } else { "" };
    Some(format!("{star}{name} {hint}").trim_end().to_string())
}

// The secret flag only written where the name alone would guess wrong
fn parse_knob_line(line: &str) -> Value {
    let (secret, rest) = match line.strip_prefix('*') {
        Some(rest) => (true, rest),
        None => (false, line),
    };
    let (name, hint) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    let mut knob = json!({ "name": name });
    if !hint.trim().is_empty() {
        knob["hint"] = json!(hint.trim());
    }
    if secret != looks_secret(name) {
        knob["secret"] = json!(secret);
    }
    knob
}

fn check_name(name: &str) -> Result<()> {
    anyhow::ensure!(
        !name.is_empty() && sanitize_name(name) == name,
        "provider name '{name}' must be lowercase letters, digits, - or _"
    );
    anyhow::ensure!(
        !RESERVED.contains(&name),
        "'{name}' is an accio command - pick another provider name"
    );
    Ok(())
}

// A built in's file may only switch it off
fn builtin_disabled(name: &str, path: &Path, value: &Value) -> Result<bool> {
    let obj = value
        .as_object()
        .with_context(|| format!("expected an object in {}", path.display()))?;
    anyhow::ensure!(
        obj.keys().all(|k| k == "disabled"),
        "'{name}' is built in - {} may only set \"disabled\"",
        path.display()
    );
    obj.get("disabled")
        .map(|d| d.as_bool().context("disabled must be true or false"))
        .transpose()
        .map(|d| d.unwrap_or(false))
}

fn preset(name: &str) -> Option<&'static str> {
    match name {
        "pi" => Some(include_str!("../presets/pi.json")),
        _ => None,
    }
}

fn skeleton(name: &str, command: &str) -> String {
    let home_env = format!("{}_HOME", name.to_ascii_uppercase().replace('-', "_"));
    let mut body = serde_json::to_string_pretty(&json!({
        "command": command,
        "home": format!("~/.{name}"),
        "home_env": home_env,
        "copy": [],
        "unset": [],
        "knobs": [
            { "name": "BASE_URL", "hint": "endpoint the harness talks to" },
            { "name": "API_KEY", "hint": "token for that endpoint", "secret": true }
        ],
        "files": {
            "merge:settings.json": { "baseUrl": "${BASE_URL}", "apiKey": "${API_KEY}" }
        }
    }))
    .unwrap();
    body.push('\n');
    body
}

impl Definition {
    fn builtin(name: &str, dir: &Path) -> Self {
        Definition {
            name: name.to_string(),
            path: dir.join(format!("{name}.json")),
            builtin: true,
            disabled: false,
            command: vec![name.to_string()],
            home: String::new(),
            home_env: None,
            copy: Vec::new(),
            unset: Vec::new(),
            env: BTreeMap::new(),
            knobs: Vec::new(),
            files: BTreeMap::new(),
        }
    }

    fn parse(name: &str, path: &Path, value: &Value) -> Result<Self> {
        let at = format!("in {}", path.display());
        check_name(name).with_context(|| at.clone())?;
        let obj = value
            .as_object()
            .with_context(|| format!("expected an object {at}"))?;
        if let Some(key) = obj.keys().find(|k| !FIELDS.contains(&k.as_str())) {
            bail!("unknown field '{key}' {at}");
        }
        let command = match obj.get("command") {
            None => vec![name.to_string()],
            Some(Value::String(s)) if !s.trim().is_empty() => vec![s.trim().to_string()],
            Some(Value::Array(items)) if !items.is_empty() => strings(items, "command", &at)?,
            Some(_) => bail!("command must be a program or an array of program and arguments {at}"),
        };
        let home = string(obj, "home", &at)?.with_context(|| format!("home is required {at}"))?;
        anyhow::ensure!(
            home.starts_with("~/") || home.starts_with('/'),
            "home must start with ~/ or / {at}"
        );
        let home = home.trim_end_matches('/').to_string();
        let copy = list(obj, "copy", &at)?;
        for item in &copy {
            anyhow::ensure!(
                relative(item),
                "copy entry '{item}' must stay inside home {at}"
            );
        }
        let mut files = BTreeMap::new();
        if let Some(raw) = obj.get("files") {
            let raw = raw
                .as_object()
                .with_context(|| format!("files must be an object {at}"))?;
            for (key, template) in raw {
                let (merge, rel) = split_merge(key);
                anyhow::ensure!(relative(rel), "file '{key}' must stay inside home {at}");
                anyhow::ensure!(
                    !merge || template.is_object(),
                    "merge file '{key}' needs an object template {at}"
                );
                files.insert(key.clone(), template.clone());
            }
        }
        let env = match obj.get("env") {
            None => BTreeMap::new(),
            Some(Value::Object(map)) => map
                .iter()
                .map(|(k, v)| {
                    v.as_str()
                        .map(|s| (k.clone(), s.to_string()))
                        .with_context(|| format!("env value '{k}' must be a string {at}"))
                })
                .collect::<Result<_>>()?,
            Some(_) => bail!("env must be an object of strings {at}"),
        };
        let disabled = obj
            .get("disabled")
            .map(|d| {
                d.as_bool()
                    .with_context(|| format!("disabled must be true or false {at}"))
            })
            .transpose()?
            .unwrap_or(false);
        let mut def = Definition {
            name: name.to_string(),
            path: path.to_path_buf(),
            builtin: false,
            disabled,
            command,
            home,
            home_env: string(obj, "home_env", &at)?,
            copy,
            unset: list(obj, "unset", &at)?,
            env,
            knobs: Vec::new(),
            files,
        };
        def.knobs = def.collect_knobs(obj.get("knobs"), &at)?;
        Ok(def)
    }

    // Declared knobs first, then every placeholder the templates mention
    fn collect_knobs(&self, declared: Option<&Value>, at: &str) -> Result<Vec<Knob>> {
        let mut knobs: Vec<Knob> = Vec::new();
        if let Some(declared) = declared {
            let items = declared
                .as_array()
                .with_context(|| format!("knobs must be an array {at}"))?;
            for item in items {
                let knob = item
                    .as_object()
                    .with_context(|| format!("each knob must be an object {at}"))?;
                let name = knob
                    .get("name")
                    .and_then(Value::as_str)
                    .with_context(|| format!("each knob needs a name {at}"))?;
                let hint = knob.get("hint").and_then(Value::as_str).unwrap_or("");
                let secret = knob
                    .get("secret")
                    .and_then(Value::as_bool)
                    .unwrap_or_else(|| looks_secret(name));
                knobs.push(Knob {
                    name: name.to_string(),
                    hint: hint.to_string(),
                    secret,
                });
            }
        }
        let mut referenced = Vec::new();
        for (key, template) in &self.files {
            refs_in(key, &mut referenced);
            references(template, &mut referenced);
        }
        for value in self.env.values() {
            refs_in(value, &mut referenced);
        }
        for name in referenced {
            if !knobs.iter().any(|k| k.name == name) {
                knobs.push(Knob {
                    secret: looks_secret(&name),
                    name,
                    hint: "used by the templates".to_string(),
                });
            }
        }
        for knob in &knobs {
            anyhow::ensure!(
                env_name(&knob.name),
                "knob '{}' must be an environment variable name {at}",
                knob.name
            );
            anyhow::ensure!(
                !matches!(knob.name.as_str(), "HOME" | "USERPROFILE" | "PATH")
                    && Some(knob.name.as_str()) != self.home_env.as_deref(),
                "knob '{}' would break the session {at}",
                knob.name
            );
        }
        Ok(knobs)
    }
}

fn string(obj: &Map<String, Value>, field: &str, at: &str) -> Result<Option<String>> {
    match obj.get(field) {
        None => Ok(None),
        Some(Value::String(s)) if !s.trim().is_empty() => Ok(Some(s.trim().to_string())),
        Some(_) => bail!("{field} must be a non empty string {at}"),
    }
}

fn list(obj: &Map<String, Value>, field: &str, at: &str) -> Result<Vec<String>> {
    match obj.get(field) {
        None => Ok(Vec::new()),
        Some(Value::Array(items)) => strings(items, field, at),
        Some(_) => bail!("{field} must be an array of strings {at}"),
    }
}

fn strings(items: &[Value], field: &str, at: &str) -> Result<Vec<String>> {
    items
        .iter()
        .map(|v| {
            v.as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .with_context(|| format!("{field} entries must be non empty strings {at}"))
        })
        .collect()
}

fn relative(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.starts_with('~')
        && path.split('/').all(|part| !part.is_empty() && part != "..")
}

fn env_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn looks_secret(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    ["key", "token", "secret", "password"]
        .iter()
        .any(|w| name.contains(w))
}

fn split_merge(key: &str) -> (bool, &str) {
    match key.strip_prefix(MERGE) {
        Some(rest) => (true, rest),
        None => (false, key),
    }
}

// Placeholders look like ${NAME} or ${NAME:json}
fn placeholder(after: &str) -> Option<(&str, bool, usize)> {
    let end = after.find('}')?;
    let inner = &after[..end];
    let (name, json) = match inner.strip_suffix(":json") {
        Some(name) => (name, true),
        None => (inner, false),
    };
    Some((name, json, end + 1))
}

fn refs_in(s: &str, out: &mut Vec<String>) {
    let mut rest = s;
    while let Some(start) = rest.find("${") {
        let after = &rest[start + 2..];
        let Some((name, _, skip)) = placeholder(after) else {
            break;
        };
        if !out.iter().any(|n| n == name) {
            out.push(name.to_string());
        }
        rest = &after[skip..];
    }
}

fn references(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::String(s) => refs_in(s, out),
        Value::Object(map) => {
            for (k, child) in map {
                refs_in(k, out);
                references(child, out);
            }
        }
        Value::Array(items) => items.iter().for_each(|i| references(i, out)),
        _ => {}
    }
}

// Placeholders filled textually, None once any of them is unset
fn fill(s: &str, values: &BTreeMap<String, String>) -> Option<String> {
    let mut out = String::new();
    let mut rest = s;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some((name, _, skip)) = placeholder(after) else {
            out.push_str(&rest[start..]);
            return Some(out);
        };
        out.push_str(values.get(name).filter(|v| !v.is_empty())?);
        rest = &after[skip..];
    }
    out.push_str(rest);
    Some(out)
}

// A lone ${NAME:json} leaf takes the value as json when it parses
fn leaf(s: &str, values: &BTreeMap<String, String>) -> Option<Value> {
    if let Some((name, true, skip)) = s.strip_prefix("${").and_then(placeholder) {
        if skip == s.len() - 2 {
            let v = values.get(name).filter(|v| !v.is_empty())?;
            return Some(serde_json::from_str(v).unwrap_or_else(|_| Value::String(v.clone())));
        }
    }
    fill(s, values).map(Value::String)
}

// Unset knobs drop their leaf, emptied containers drop with them
fn render(template: &Value, values: &BTreeMap<String, String>) -> Option<Value> {
    match template {
        Value::String(s) => leaf(s, values),
        Value::Object(map) => {
            let mut out = Map::new();
            for (k, child) in map {
                let Some(key) = fill(k, values) else { continue };
                if let Some(v) = render(child, values) {
                    out.insert(key, v);
                }
            }
            (map.is_empty() || !out.is_empty()).then(|| Value::Object(out))
        }
        Value::Array(items) => {
            let out: Vec<Value> = items.iter().filter_map(|i| render(i, values)).collect();
            (items.is_empty() || !out.is_empty()).then_some(Value::Array(out))
        }
        other => Some(other.clone()),
    }
}

fn parse_dotenv(body: &str) -> BTreeMap<String, String> {
    body.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect()
}

struct Custom {
    def: Definition,
    // Live home as profiles key it, ~/ kept so profiles travel between machines
    home_key: String,
}

impl Custom {
    fn new(def: &Definition) -> Result<Self> {
        anyhow::ensure!(!def.builtin, "'{}' is built in", def.name);
        let home_key = def
            .home_env
            .as_deref()
            .and_then(|v| std::env::var(v).ok())
            .map(|v| v.trim_end_matches('/').to_string())
            .filter(|v| v.starts_with('/') || v.starts_with("~/"))
            .unwrap_or_else(|| def.home.clone());
        Ok(Custom {
            def: def.clone(),
            home_key,
        })
    }

    fn env_key(&self) -> String {
        format!("{}/{ENV_FILE}", self.home_key)
    }

    // The live profile's rendering leaves the copy, never the live home itself
    fn lift_live(&self, home: &Path, dir: &Path) -> Result<()> {
        let Ok(body) = fs::read_to_string(home.join(ENV_FILE)) else {
            return Ok(());
        };
        let live = parse_dotenv(&body);
        for (key, template) in &self.def.files {
            let (merge, rel) = split_merge(key);
            let Some(rendered) = render(template, &live) else {
                continue;
            };
            let target = dir.join(rel);
            if merge {
                session::unmerge_object(&target, &rendered)?;
            } else if target.exists() {
                fs::remove_file(&target)
                    .with_context(|| format!("cant remove {}", target.display()))?;
            }
        }
        Ok(())
    }

    // Where a stored entry lands under a session directory
    fn relative<'a>(&self, path: &'a str) -> Option<&'a str> {
        [self.home_key.as_str(), self.def.home.as_str()]
            .iter()
            .find_map(|home| path.strip_prefix(home)?.strip_prefix('/'))
            .filter(|rel| relative(rel))
    }
}

impl Backend for Custom {
    fn name(&self) -> &str {
        &self.def.name
    }

    fn read_live(&self) -> BTreeMap<String, String> {
        BTreeMap::new()
    }

    fn write_live(&self, _contents: &BTreeMap<String, String>) -> Result<()> {
        Ok(())
    }

    fn login(&self) -> &[&str] {
        &[]
    }

    fn fetch(&self, files: BTreeMap<String, String>) -> Job {
        files::facts_job(files)
    }

    fn info(&self) -> Vec<(String, String)> {
        let files: Vec<&str> = self.def.files.keys().map(String::as_str).collect();
        vec![
            ("command".to_string(), self.def.command.join(" ")),
            (
                "home".to_string(),
                match &self.def.home_env {
                    Some(env) => format!("{} ({env})", self.home_key),
                    None => self.home_key.clone(),
                },
            ),
            (
                "files".to_string(),
                if files.is_empty() {
                    "-".to_string()
                } else {
                    files.join("  ")
                },
            ),
            ("defined".to_string(), self.def.path.display().to_string()),
        ]
    }

    fn knobs(&self) -> Vec<Knob> {
        self.def.knobs.clone()
    }

    fn is_login(&self, _plain: &BTreeMap<String, String>) -> bool {
        false
    }

    // Knob values as a dotenv in home beside whatever the templates render
    fn compose(&self, values: &BTreeMap<String, String>) -> BTreeMap<String, String> {
        let mut bag = files::compose_dotenv(&self.env_key(), values);
        for (key, template) in &self.def.files {
            let (merge, rel) = split_merge(key);
            let Some(rendered) = render(template, values) else {
                continue;
            };
            let content = match (&rendered, merge) {
                (_, true) => canon_str(&rendered.to_string()),
                (Value::String(s), false) => s.clone(),
                (_, false) => format!("{}\n", serde_json::to_string_pretty(&rendered).unwrap()),
            };
            let prefix = if merge { MERGE } else { "" };
            bag.insert(format!("{prefix}{}/{rel}", self.home_key), content);
        }
        bag
    }

    fn decompose(&self, files: &BTreeMap<String, String>) -> Option<BTreeMap<String, String>> {
        files
            .iter()
            .find(|(k, _)| !k.starts_with(MERGE) && k.ends_with(&format!("/{ENV_FILE}")))
            .map(|(_, body)| parse_dotenv(body))
    }

    // The live home copied, the profile laid over the copy, every knob in the environment
    fn session(&self, files: &BTreeMap<String, String>, dir: &Path) -> Result<Command> {
        let home_env = self.def.home_env.as_deref().with_context(|| {
            format!(
                "isolated {} sessions need home_env in {}",
                self.def.name,
                self.def.path.display()
            )
        })?;
        let copy: Vec<&str> = self.def.copy.iter().map(String::as_str).collect();
        let home = expand(&self.home_key);
        session::copy_resources(&home, dir, &copy)?;
        self.lift_live(&home, dir)?;
        for (key, content) in files {
            let (merge, path) = split_merge(key);
            let rel = self
                .relative(path)
                .with_context(|| format!("profile entry '{key}' is outside {}", self.home_key))?;
            let target = dir.join(rel);
            if merge {
                let patch: Value = serde_json::from_str(content)
                    .with_context(|| format!("invalid saved content for '{key}'"))?;
                session::merge_object(&target, &patch)?;
            } else {
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)?;
                }
                write_atomic(&target, content.as_bytes())?;
            }
        }
        let values = self.decompose(files).unwrap_or_default();
        let mut command = Command::new(&self.def.command[0]);
        command.args(&self.def.command[1..]);
        for key in &self.def.unset {
            command.env_remove(key);
        }
        for (key, value) in &values {
            command.env(key, value);
        }
        for (key, template) in &self.def.env {
            if let Some(value) = fill(template, &values) {
                command.env(key, value);
            }
        }
        command.env(home_env, dir);
        Ok(command)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("accio-custom-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn values(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn pi(path: &Path) -> Definition {
        Definition::parse(
            "pi",
            path,
            &serde_json::from_str(preset("pi").unwrap()).unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn placeholders_fill_type_and_prune() {
        let v = values(&[("A", "x"), ("N", "42"), ("S", "not json"), ("E", "")]);
        assert_eq!(fill("${A}-${N}", &v).unwrap(), "x-42");
        assert_eq!(fill("$A stays and ${", &v).unwrap(), "$A stays and ${");
        assert!(fill("${MISSING}", &v).is_none());
        assert!(fill("${E}", &v).is_none());
        assert_eq!(leaf("${N:json}", &v).unwrap(), json!(42));
        assert_eq!(leaf("${S:json}", &v).unwrap(), json!("not json"));
        assert_eq!(leaf("${N:json}!", &v).unwrap(), json!("42!"));
        let template = json!({
            "providers": {"${A}": {"url": "${A}", "models": [{"id": "${MISSING}", "n": "${N:json}"}]}},
            "gone": {"${MISSING}": 1},
            "list": ["${MISSING}"],
            "keep": true
        });
        assert_eq!(
            render(&template, &v).unwrap(),
            json!({"providers": {"x": {"url": "x", "models": [{"n": 42}]}}, "keep": true})
        );
        assert!(render(&json!({"a": "${MISSING}"}), &v).is_none());
        assert_eq!(render(&json!({}), &v).unwrap(), json!({}));
    }

    #[test]
    fn definitions_validate_and_discover_knobs() {
        let path = PathBuf::from("/tmp/pi.json");
        let def = pi(&path);
        let names: Vec<&str> = def.knobs.iter().map(|k| k.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "PROVIDER",
                "BASE_URL",
                "API",
                "API_KEY",
                "MODEL",
                "CONTEXT_WINDOW",
                "MAX_TOKENS"
            ]
        );
        assert!(
            def.knobs
                .iter()
                .find(|k| k.name == "API_KEY")
                .unwrap()
                .secret
        );
        assert_eq!(def.home_env.as_deref(), Some("PI_CODING_AGENT_DIR"));
        assert_eq!(def.command, ["pi"]);

        let parse = |v: Value| Definition::parse("x", &path, &v);
        let discovered = parse(
            json!({"home": "~/.x", "files": {"a.json": {"k": "${TOKEN}"}},
            "env": {"Y": "${Z}"}}),
        )
        .unwrap();
        let names: Vec<(String, bool)> = discovered
            .knobs
            .iter()
            .map(|k| (k.name.clone(), k.secret))
            .collect();
        assert_eq!(
            names,
            [("TOKEN".to_string(), true), ("Z".to_string(), false)]
        );
        assert_eq!(discovered.command, ["x"]);
        assert!(parse(json!({"home": "~/.x", "nope": 1})).is_err());
        assert!(parse(json!({"command": "x"})).is_err());
        assert!(parse(json!({"home": "relative"})).is_err());
        assert!(parse(json!({"home": "~/.x", "files": {"/etc/x": "a"}})).is_err());
        assert!(parse(json!({"home": "~/.x", "files": {"merge:../x.json": {}}})).is_err());
        assert!(parse(json!({"home": "~/.x", "files": {"merge:x.json": "text"}})).is_err());
        assert!(parse(json!({"home": "~/.x", "files": {"x": "${bad-name}"}})).is_err());
        assert!(parse(json!({"home": "~/.x", "knobs": [{"name": "HOME"}]})).is_err());
        assert!(parse(
            json!({"home": "~/.x", "home_env": "X_HOME", "knobs": [{"name": "X_HOME"}]})
        )
        .is_err());
        assert!(
            parse(json!({"home": "~/.x", "command": ["x", "--flag"], "disabled": true}))
                .is_ok_and(|d| d.disabled && d.command == ["x", "--flag"])
        );
        assert!(Definition::parse("list", &path, &json!({"home": "~/.x"})).is_err());
        assert!(Definition::parse("Bad Name", &path, &json!({"home": "~/.x"})).is_err());
    }

    #[test]
    fn compose_keys_under_home_and_decompose_returns_knobs() {
        let def = pi(Path::new("/tmp/pi.json"));
        let custom = Custom::new(&def).unwrap();
        let v = values(&[
            ("PROVIDER", "nebu"),
            ("BASE_URL", "http://nebu:8484/v1"),
            ("API", "openai-completions"),
            ("API_KEY", "sk-x"),
            ("MODEL", "freddie"),
            ("CONTEXT_WINDOW", "131072"),
        ]);
        let bag = custom.compose(&v);
        let keys: Vec<&String> = bag.keys().collect();
        assert_eq!(
            keys,
            [
                "merge:~/.pi/agent/models.json",
                "merge:~/.pi/agent/settings.json",
                "~/.pi/agent/.accio.env"
            ]
        );
        let models: Value = serde_json::from_str(&bag["merge:~/.pi/agent/models.json"]).unwrap();
        assert_eq!(
            models,
            json!({"providers": {"nebu": {"api": "openai-completions", "apiKey": "sk-x",
                "baseUrl": "http://nebu:8484/v1",
                "models": [{"contextWindow": 131072, "id": "freddie"}]}}})
        );
        assert_eq!(
            bag["merge:~/.pi/agent/settings.json"],
            r#"{"defaultModel":"freddie","defaultProvider":"nebu"}"#
        );
        assert_eq!(custom.decompose(&bag).unwrap(), v);
        let partial = custom.compose(&values(&[("API_KEY", "k")]));
        assert_eq!(partial.len(), 1, "nothing renders without a provider");
        assert!(partial.contains_key("~/.pi/agent/.accio.env"));
    }

    #[test]
    fn session_copies_home_lays_profile_and_exports_knobs() {
        let root = tdir("session");
        let home = root.join("home");
        fs::create_dir_all(home.join("extensions")).unwrap();
        fs::write(home.join("extensions/x.js"), "x").unwrap();
        fs::write(
            home.join("models.json"),
            r#"{"providers":{"mine":{"baseUrl":"keep"},"old":{"apiKey":"sk-old","extra":1}}}"#,
        )
        .unwrap();
        fs::write(
            home.join("auth.json"),
            r#"{"anthropic":{"type":"api_key"}}"#,
        )
        .unwrap();
        fs::write(
            home.join(ENV_FILE),
            "PROVIDER=old\nAPI_KEY=sk-old\nMODEL=m\n",
        )
        .unwrap();
        fs::write(home.join("notes.txt"), "model m\n").unwrap();
        let def = Definition::parse(
            "fake",
            &root.join("fake.json"),
            &json!({
                "command": ["fake-cli", "--quiet"],
                "home": home.display().to_string(),
                "home_env": "FAKE_HOME",
                "copy": ["models.json", "auth.json", "extensions", "notes.txt", "missing"],
                "unset": ["OPENAI_API_KEY"],
                "env": {"FAKE_MODEL": "${PROVIDER}/${MODEL}"},
                "files": {
                    "merge:models.json": {"providers": {"${PROVIDER}": {"apiKey": "${API_KEY}"}}},
                    "notes.txt": "model ${MODEL}\n"
                }
            }),
        )
        .unwrap();
        let custom = Custom::new(&def).unwrap();
        let bag = custom.compose(&values(&[
            ("PROVIDER", "nebu"),
            ("API_KEY", "sk-x"),
            ("MODEL", "freddie"),
        ]));
        let dir = root.join("session");
        fs::create_dir_all(&dir).unwrap();
        let command = custom.session(&bag, &dir).unwrap();

        assert_eq!(command.get_program(), "fake-cli");
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(args, ["--quiet"]);
        let envs: BTreeMap<String, Option<String>> = command
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect();
        assert_eq!(envs["FAKE_HOME"].as_deref(), Some(dir.to_str().unwrap()));
        assert_eq!(envs["API_KEY"].as_deref(), Some("sk-x"));
        assert_eq!(envs["FAKE_MODEL"].as_deref(), Some("nebu/freddie"));
        assert_eq!(envs["OPENAI_API_KEY"], None);

        let models: Value =
            serde_json::from_slice(&fs::read(dir.join("models.json")).unwrap()).unwrap();
        assert_eq!(
            models,
            json!({"providers": {"mine": {"baseUrl": "keep"}, "old": {"extra": 1},
                "nebu": {"apiKey": "sk-x"}}}),
            "the live profile's leaves leave the copy, hand added ones stay"
        );
        assert_eq!(
            fs::read_to_string(dir.join("notes.txt")).unwrap(),
            "model freddie\n"
        );
        assert_eq!(
            fs::read_to_string(dir.join("extensions/x.js")).unwrap(),
            "x"
        );
        assert!(dir.join("auth.json").exists());
        assert!(dir.join(ENV_FILE).exists());
        assert_eq!(
            fs::read_to_string(home.join("models.json")).unwrap(),
            r#"{"providers":{"mine":{"baseUrl":"keep"},"old":{"apiKey":"sk-old","extra":1}}}"#,
            "the live home is never written"
        );
        assert_eq!(
            fs::read_to_string(home.join("notes.txt")).unwrap(),
            "model m\n"
        );

        let mut foreign = bag.clone();
        foreign.insert("/elsewhere/x".into(), "x".into());
        assert!(custom.session(&foreign, &root.join("other")).is_err());
        let mut nohome = def.clone();
        nohome.home_env = None;
        assert!(Custom::new(&nohome).unwrap().session(&bag, &dir).is_err());
    }

    #[test]
    fn registry_adds_removes_and_toggles() {
        let dir = tdir("registry");
        let registry = Registry::load_at(dir.clone()).unwrap();
        let names: Vec<&str> = registry
            .definitions
            .iter()
            .map(|d| d.name.as_str())
            .collect();
        assert_eq!(names, BUILTIN);
        assert!(registry.add("claude", None).is_err());
        assert!(registry.add("list", None).is_err());
        let path = registry.add("pi", None).unwrap();
        assert_eq!(path, dir.join("pi.json"));
        assert!(registry.add("pi", None).is_err());
        registry.add("zed", Some("zed-cli")).unwrap();
        registry.set_disabled("claude", true).unwrap();
        assert_eq!(
            fs::read_to_string(dir.join("claude.json")).unwrap().trim(),
            "{\n  \"disabled\": true\n}"
        );
        assert!(registry.set_disabled("nope", true).is_err());

        let registry = Registry::load_at(dir.clone()).unwrap();
        let names: Vec<&str> = registry
            .definitions
            .iter()
            .map(|d| d.name.as_str())
            .collect();
        assert_eq!(names, ["claude", "codex", "grok", "gemini", "pi", "zed"]);
        assert!(registry.get("claude").unwrap().disabled);
        let enabled: Vec<&str> = registry.enabled().map(|d| d.name.as_str()).collect();
        assert_eq!(enabled, ["codex", "grok", "gemini", "pi", "zed"]);
        let zed = registry.get("zed").unwrap();
        assert_eq!(zed.command, ["zed-cli"]);
        assert_eq!(zed.home_env.as_deref(), Some("ZED_HOME"));
        assert!(!registry.get("pi").unwrap().knobs.is_empty());

        registry.set_disabled("claude", false).unwrap();
        assert!(!dir.join("claude.json").exists());
        registry.set_disabled("zed", true).unwrap();
        assert!(registry.remove("claude").is_err());
        registry.remove("zed").unwrap();
        let registry = Registry::load_at(dir.clone()).unwrap();
        assert!(registry.get("zed").is_none());
        assert!(!registry.get("claude").unwrap().disabled);

        fs::write(
            dir.join("codex.json"),
            r#"{"disabled": true, "home": "~/.codex"}"#,
        )
        .unwrap();
        assert!(Registry::load_at(dir.clone()).is_err());
        fs::write(dir.join("codex.json"), "{}").unwrap();
        fs::write(dir.join("Bad Name.json"), r#"{"home": "~/.x"}"#).unwrap();
        assert!(Registry::load_at(dir).is_err());
    }

    #[test]
    fn drafts_round_trip_definitions_and_save_keeps_disabled() {
        let dir = tdir("draft");
        let registry = Registry::load_at(dir.clone()).unwrap();
        assert!(registry.draft("claude").is_err());
        assert!(registry.draft("nope").is_err());
        registry.add("pi", None).unwrap();
        let registry = Registry::load_at(dir.clone()).unwrap();
        let draft = registry.draft("pi").unwrap();
        assert_eq!(draft.command, "pi");
        assert_eq!(draft.home, "~/.pi/agent");
        assert_eq!(draft.home_env, "PI_CODING_AGENT_DIR");
        assert!(draft.copy.starts_with("settings.json, auth.json, "));
        assert_eq!(draft.unset, "");
        assert!(draft.env.is_empty());
        assert_eq!(
            draft.knobs[0],
            "PROVIDER provider id pi lists the models under"
        );
        assert_eq!(draft.knobs[3], "*API_KEY bearer token for that endpoint");
        assert_eq!(draft.files.len(), 2);
        assert!(draft.files[0].starts_with("merge:models.json {\"providers\":{\"${PROVIDER}\":"));
        assert_eq!(
            draft.files[1],
            "merge:settings.json {\"defaultProvider\":\"${PROVIDER}\",\"defaultModel\":\"${MODEL}\"}"
        );

        registry.set_disabled("pi", true).unwrap();
        let mut edited = draft.clone();
        edited.command = "pi --fast".into();
        edited.env = vec!["PI_THEME=dark".into(), "".into()];
        edited.knobs.push("*AUTH bearer header".into());
        edited.knobs.push("TOKEN_KIND not actually hidden".into());
        edited.files.push("notes.md \"model ${MODEL}\"".into());
        let path = registry.save(&edited).unwrap();
        assert_eq!(path, dir.join("pi.json"));
        let saved: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved["command"], json!(["pi", "--fast"]));
        assert_eq!(saved["env"], json!({ "PI_THEME": "dark" }));
        assert_eq!(saved["disabled"], json!(true));
        assert_eq!(saved["files"]["notes.md"], json!("model ${MODEL}"));
        let knobs = saved["knobs"].as_array().unwrap();
        assert_eq!(
            knobs[3],
            json!({ "name": "API_KEY", "hint": "bearer token for that endpoint" })
        );
        assert_eq!(
            knobs[7],
            json!({ "name": "AUTH", "hint": "bearer header", "secret": true })
        );
        assert_eq!(
            knobs[8],
            json!({ "name": "TOKEN_KIND", "hint": "not actually hidden", "secret": false })
        );
        let reloaded = Registry::load_at(dir.clone()).unwrap();
        let def = reloaded.get("pi").unwrap();
        assert!(def.disabled);
        assert_eq!(def.command, ["pi", "--fast"]);
        assert!(def.knobs.iter().any(|k| k.name == "AUTH" && k.secret));
        assert!(def
            .knobs
            .iter()
            .any(|k| k.name == "TOKEN_KIND" && !k.secret));
        assert_eq!(reloaded.draft("pi").unwrap(), edited_clean(&edited));

        let fresh = Draft {
            name: "zed".into(),
            home: "~/.zed".into(),
            ..Draft::default()
        };
        registry.save(&fresh).unwrap();
        assert_eq!(
            fs::read_to_string(dir.join("zed.json")).unwrap(),
            "{\n  \"home\": \"~/.zed\"\n}\n"
        );
        let mut bad = fresh.clone();
        bad.files = vec!["x.json".into()];
        assert!(registry.save(&bad).is_err());
        bad.files = vec!["x.json not json".into()];
        assert!(registry.save(&bad).is_err());
        bad.files = vec!["/etc/x.json {}".into()];
        assert!(registry.save(&bad).is_err());
        bad.files.clear();
        bad.env = vec!["NOEQUALS".into()];
        assert!(registry.save(&bad).is_err());
        bad.env.clear();
        bad.name = "claude".into();
        assert!(registry.save(&bad).is_err());
        bad.name = "Bad Name".into();
        assert!(registry.save(&bad).is_err());
    }

    // What a draft looks like after its blank rows are dropped
    fn edited_clean(d: &Draft) -> Draft {
        let keep = |rows: &[String]| -> Vec<String> {
            rows.iter()
                .filter(|r| !r.trim().is_empty())
                .cloned()
                .collect()
        };
        Draft {
            env: keep(&d.env),
            knobs: keep(&d.knobs),
            files: keep(&d.files),
            ..d.clone()
        }
    }
}
