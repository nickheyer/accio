//! Live credentials as plain files on disk - what most provider clis do

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::{Map, Value};

use crate::fsutil::{expand, write_atomic};
use crate::usage::{parse_usage, Usage};
use crate::{Job, Outcome};

pub fn read_live(paths: &[&str]) -> BTreeMap<String, String> {
    let mut live = BTreeMap::new();
    for cfg in paths {
        if let Ok(content) = fs::read_to_string(expand(cfg)) {
            live.insert(cfg.to_string(), content);
        }
    }
    live
}

// clear every configured path, then lay down the given contents
pub fn write_live(paths: &[&str], contents: &BTreeMap<String, String>) -> Result<()> {
    for cfg in paths {
        let path = expand(cfg);
        if path.exists() {
            fs::remove_file(&path).with_context(|| format!("cant remove {}", path.display()))?;
        }
    }
    for (cfg, content) in contents {
        let path = expand(cfg);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("cant create {}", parent.display()))?;
        }
        write_atomic(&path, content.as_bytes())?;
    }
    Ok(())
}

pub fn facts_job(files: BTreeMap<String, String>) -> Job {
    Box::new(move || Outcome { usage: Ok(facts(&files)), state: None })
}

pub fn info(paths: &[&str], login: &[&str]) -> Vec<(String, String)> {
    vec![
        (
            "files".to_string(),
            if paths.is_empty() { "-".to_string() } else { paths.join("  ") },
        ),
        (
            "login".to_string(),
            if login.is_empty() { "-".to_string() } else { login.join(" ") },
        ),
    ]
}

fn facts(files: &BTreeMap<String, String>) -> Usage {
    let mut root = Map::new();
    for (path, content) in files {
        let v: Value = match serde_json::from_str(content) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let stem = Path::new(path)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("file")
            .to_string();
        root.insert(stem, redact(&v));
    }
    parse_usage(&Value::Object(root))
}

// Knob values as one flat json object file
pub fn compose_json(path: &str, values: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    let obj: Map<String, Value> =
        values.iter().map(|(k, v)| (k.clone(), Value::from(v.as_str()))).collect();
    BTreeMap::from([(path.to_string(), Value::Object(obj).to_string())])
}

// Knob values as KEY=VALUE lines
pub fn compose_dotenv(path: &str, values: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    let body: String = values.iter().map(|(k, v)| format!("{k}={v}\n")).collect();
    BTreeMap::from([(path.to_string(), body)])
}

// creds files are wall to wall secrets - facts only keep what survives this
fn redact(v: &Value) -> Value {
    const SECRET_KEYS: &[&str] =
        &["token", "secret", "password", "credential", "key", "auth", "cookie"];
    match v {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(k, _)| {
                    let k = k.to_ascii_lowercase();
                    !SECRET_KEYS.iter().any(|w| k.contains(w))
                })
                .map(|(k, c)| (k.clone(), redact(c)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(redact).collect()),
        Value::String(s) if s.len() > 80 => Value::Null,
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn facts_hide_secrets() {
        let mut files = BTreeMap::new();
        files.insert(
            "~/.x/auth.json".to_string(),
            r#"{"OPENAI_API_KEY":"sk-verysecret","tokens":{"id_token":"a.b.c"},"last_refresh":"2026-08-01T00:00:00Z","plan":"plus"}"#
                .to_string(),
        );
        let u = facts(&files);
        assert!(u.facts.iter().all(|f| !f.value.contains("sk-verysecret")));
        assert!(u.facts.iter().all(|f| !f.value.contains("a.b.c")));
        assert!(u.facts.iter().any(|f| f.label == "auth · plan" && f.value == "plus"));
    }
}
