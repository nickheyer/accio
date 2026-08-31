//! claude provider - swaps claude code's live credentials, usage from the oauth endpoint

mod oauth;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use accio_provider::{
    files, parse_usage, read_json, write_atomic, Backend, Job, Knob, Outcome, Provider, Swap,
};
use anyhow::{Context, Result};
use serde_json::{json, Value};

#[cfg(target_os = "macos")]
const KEYCHAIN_SERVICE: &str = "Claude Code-credentials";

pub fn provider() -> Result<impl Provider> {
    migrate_flat_profiles();
    Swap::load(Claude::locate()?)
}

struct Claude {
    creds_path: PathBuf,
    claude_json_path: PathBuf,
    settings_key: String,
}

impl Claude {
    fn locate() -> Result<Self> {
        let home = dirs::home_dir().context("cant determine home dir")?;
        let (claude_dir, claude_json_path, settings_key) =
            match std::env::var_os("CLAUDE_CONFIG_DIR") {
                Some(dir) => {
                    let dir = PathBuf::from(dir);
                    let json = dir.join(".claude.json");
                    let settings = dir.join("settings.json").display().to_string();
                    (dir, json, settings)
                }
                None => (
                    home.join(".claude"),
                    home.join(".claude.json"),
                    "~/.claude/settings.json".into(),
                ),
            };
        Ok(Claude {
            creds_path: claude_dir.join(".credentials.json"),
            claude_json_path,
            settings_key,
        })
    }
}

impl Backend for Claude {
    fn name(&self) -> &str {
        "claude"
    }

    fn read_live(&self) -> BTreeMap<String, String> {
        let mut live = BTreeMap::new();
        let creds = match read_live_creds(&self.creds_path) {
            Some(v) => v,
            None => return live,
        };
        live.insert("credentials".to_string(), creds.to_string());
        if let Some(acct) = read_json(&self.claude_json_path)
            .and_then(|v| v.get("oauthAccount").cloned())
            .filter(|a| !a.is_null())
        {
            live.insert("oauth_account".to_string(), acct.to_string());
        }
        live
    }

    // creds go to the keychain/file wholesale, the account only merges into .claude.json
    fn write_live(&self, contents: &BTreeMap<String, String>) -> Result<()> {
        match contents.get("credentials") {
            Some(body) => write_live_creds(&self.creds_path, body)?,
            None => clear_live_creds(&self.creds_path)?,
        }
        match contents.get("oauth_account") {
            Some(acct) => {
                let account: Value = serde_json::from_str(acct)?;
                let mut claude_json =
                    read_json(&self.claude_json_path).unwrap_or_else(|| json!({}));
                if let Some(obj) = claude_json.as_object_mut() {
                    obj.insert("oauthAccount".to_string(), account);
                    write_atomic(
                        &self.claude_json_path,
                        serde_json::to_string(&claude_json)?.as_bytes(),
                    )?;
                }
            }
            // stale account identity must not shadow a configured profile
            None => {
                if let Some(mut claude_json) = read_json(&self.claude_json_path) {
                    let removed = claude_json
                        .as_object_mut()
                        .is_some_and(|o| o.remove("oauthAccount").is_some());
                    if removed {
                        write_atomic(
                            &self.claude_json_path,
                            serde_json::to_string(&claude_json)?.as_bytes(),
                        )?;
                    }
                }
            }
        }
        Ok(())
    }

    fn login(&self) -> &[&str] {
        &["claude", "/login"]
    }

    fn knobs(&self) -> Vec<Knob> {
        vec![
            Knob::new("ANTHROPIC_BASE_URL", "endpoint claude code talks to"),
            Knob::secret("ANTHROPIC_AUTH_TOKEN", "bearer token for that endpoint"),
            Knob::secret("ANTHROPIC_API_KEY", "x-api-key auth instead of bearer"),
            Knob::new("ANTHROPIC_MODEL", "model override"),
            Knob::new("ANTHROPIC_SMALL_FAST_MODEL", "background model override"),
        ]
    }

    // configured profiles ride the env block of settings.json
    fn compose(&self, values: &BTreeMap<String, String>) -> BTreeMap<String, String> {
        BTreeMap::from([(
            format!("merge:{}", self.settings_key),
            json!({ "env": values }).to_string(),
        )])
    }

    fn fetch(&self, files: BTreeMap<String, String>) -> Job {
        Box::new(move || {
            let mut creds: Value = match files
                .get("credentials")
                .and_then(|s| serde_json::from_str(s).ok())
            {
                Some(v) => v,
                // configured profiles have no oauth, show their facts instead
                None => return files::facts_job(files)(),
            };
            let refreshed = match oauth::ensure_fresh(&mut creds) {
                Ok(r) => r,
                Err(e) => {
                    return Outcome {
                        usage: Err(format!("{e:#}")),
                        state: None,
                    }
                }
            };
            let token = creds
                .pointer("/claudeAiOauth/accessToken")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let usage = match oauth::fetch_usage(&token) {
                Ok(v) => {
                    let u = parse_usage(&v);
                    if u.windows.is_empty() && u.facts.is_empty() {
                        Err("no usage data in response".to_string())
                    } else {
                        Ok(u)
                    }
                }
                Err(e) => Err(format!("{e:#}")),
            };
            Outcome {
                usage,
                state: refreshed.then(|| json!({ "credentials": creds.to_string() })),
            }
        })
    }
}

// profiles saved before providers had their own directories - copied forward, originals stay put
fn migrate_flat_profiles() {
    let accounts = match dirs::config_dir() {
        Some(d) => d.join("accio").join("accounts"),
        None => return,
    };
    let entries = match fs::read_dir(&accounts) {
        Ok(e) => e,
        Err(_) => return,
    };
    let dir = accounts.join("claude");
    for path in entries.flatten().map(|e| e.path()) {
        if !path.is_file() || path.extension().map_or(true, |e| e != "json") {
            continue;
        }
        let name = match path.file_stem().and_then(|s| s.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        let v = match read_json(&path) {
            Some(v) => v,
            None => continue,
        };
        let creds = match v.get("credentials") {
            Some(c) if !c.is_null() => c,
            _ => continue,
        };
        let target = dir.join(format!("{name}.json"));
        if target.exists() || fs::create_dir_all(&dir).is_err() {
            continue;
        }
        let mut files = serde_json::Map::new();
        files.insert("credentials".to_string(), Value::from(creds.to_string()));
        if let Some(acct) = v.get("oauth_account").filter(|a| !a.is_null()) {
            files.insert("oauth_account".to_string(), Value::from(acct.to_string()));
        }
        let body = match serde_json::to_string_pretty(&json!({ "files": files })) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let _ = write_atomic(&target, body.as_bytes());
    }
}

// Live cc creds sit in the keychain on mac
fn read_live_creds(creds_path: &Path) -> Option<Value> {
    #[cfg(target_os = "macos")]
    if let Some(v) = keychain_read() {
        return Some(v);
    }
    read_json(creds_path)
}

fn write_live_creds(creds_path: &Path, body: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    if keychain_write(body).is_ok() {
        // Stale file must not shadow the keychain
        let _ = fs::remove_file(creds_path);
        return Ok(());
    }
    write_atomic(creds_path, body.as_bytes())
}

fn clear_live_creds(creds_path: &Path) -> Result<()> {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("security")
        .args(["delete-generic-password", "-s", KEYCHAIN_SERVICE])
        .output();
    if creds_path.exists() {
        fs::remove_file(creds_path)?;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn keychain_read() -> Option<Value> {
    let out = std::process::Command::new("security")
        .args(["find-generic-password", "-s", KEYCHAIN_SERVICE, "-w"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).ok()
}

#[cfg(target_os = "macos")]
fn keychain_write(body: &str) -> Result<()> {
    let user = std::env::var("USER").unwrap_or_else(|_| "unknown".to_string());
    let out = std::process::Command::new("security")
        .args([
            "add-generic-password",
            "-U",
            "-a",
            &user,
            "-s",
            KEYCHAIN_SERVICE,
            "-w",
            body,
        ])
        .output()
        .context("cant run `security`")?;
    if !out.status.success() {
        anyhow::bail!("keychain write failed - is the login keychain unlocked?");
    }
    Ok(())
}
