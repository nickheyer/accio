//! Profile store plus live claude credentials for swap

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

#[cfg(target_os = "macos")]
const KEYCHAIN_SERVICE: &str = "Claude Code-credentials";

pub struct Profile {
    pub name: String,
    pub credentials: Value,
    pub oauth_account: Value,
}

impl Profile {
    pub fn email(&self) -> Option<&str> {
        self.oauth_account.get("emailAddress").and_then(Value::as_str)
    }

    pub fn account_uuid(&self) -> Option<&str> {
        self.oauth_account.get("accountUuid").and_then(Value::as_str)
    }

    pub fn subscription(&self) -> Option<&str> {
        self.credentials
            .get("claudeAiOauth")
            .and_then(|o| o.get("subscriptionType"))
            .and_then(Value::as_str)
    }

    pub fn access_token(&self) -> Option<&str> {
        self.credentials
            .get("claudeAiOauth")
            .and_then(|o| o.get("accessToken"))
            .and_then(Value::as_str)
    }
}

pub struct Store {
    pub profiles: Vec<Profile>,
    pub active: Option<usize>,
    accounts_dir: PathBuf,
    creds_path: PathBuf,
    claude_json_path: PathBuf,
}

impl Store {
    pub fn load() -> Result<Self> {
        let home = dirs::home_dir().context("cant determine home dir")?;
        let (claude_dir, claude_json_path) = match std::env::var_os("CLAUDE_CONFIG_DIR") {
            Some(dir) => {
                let dir = PathBuf::from(dir);
                let json = dir.join(".claude.json");
                (dir, json)
            }
            None => (home.join(".claude"), home.join(".claude.json")),
        };
        let accounts_dir = dirs::config_dir()
            .context("cant determine config dir")?
            .join("accio")
            .join("accounts");
        fs::create_dir_all(&accounts_dir)
            .with_context(|| format!("cant create {}", accounts_dir.display()))?;

        let mut store = Store {
            profiles: Vec::new(),
            active: None,
            accounts_dir,
            creds_path: claude_dir.join(".credentials.json"),
            claude_json_path,
        };

        for entry in fs::read_dir(&store.accounts_dir)? {
            let path = entry?.path();
            if path.extension().map_or(true, |e| e != "json") {
                continue;
            }
            let name = match path.file_stem().and_then(|s| s.to_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            let v: Value = match read_json(&path) {
                Some(v) => v,
                None => continue, // skip borked profiles, dont die
            };
            store.profiles.push(Profile {
                name,
                credentials: v.get("credentials").cloned().unwrap_or(Value::Null),
                oauth_account: v.get("oauth_account").cloned().unwrap_or(Value::Null),
            });
        }
        store.profiles.sort_by(|a, b| a.name.cmp(&b.name));
        store.absorb_live()?;
        Ok(store)
    }

    // Read live cc creds - if tracked profile, swap in, else import new tracked profile and set active
    pub fn absorb_live(&mut self) -> Result<()> {
        self.active = None;
        let live_creds = match read_live_creds(&self.creds_path) {
            Some(v) => v,
            None => return Ok(()),
        };
        let live_account = read_json(&self.claude_json_path)
            .and_then(|v| v.get("oauthAccount").cloned())
            .unwrap_or(Value::Null);
        let live_uuid = live_account.get("accountUuid").and_then(Value::as_str);
        let live_token = live_creds
            .get("claudeAiOauth")
            .and_then(|o| o.get("accessToken"))
            .and_then(Value::as_str);

        let matched = self.profiles.iter().position(|p| match (live_uuid, p.account_uuid()) {
            (Some(a), Some(b)) => a == b,
            _ => live_token.is_some() && live_token == p.access_token(),
        });

        if let Some(idx) = matched {
            let p = &mut self.profiles[idx];
            if p.credentials != live_creds
                || (!live_account.is_null() && p.oauth_account != live_account)
            {
                p.credentials = live_creds;
                if !live_account.is_null() {
                    p.oauth_account = live_account;
                }
                let p = &self.profiles[idx];
                save_profile(&self.accounts_dir, p)?;
            }
            self.active = Some(idx);
        } else {
            let base = live_account
                .get("emailAddress")
                .and_then(Value::as_str)
                .map(|e| e.split('@').next().unwrap_or(e).to_string())
                .unwrap_or_else(|| "account".to_string());
            let name = self.unique_name(&sanitize_name(&base));
            let profile = Profile {
                name: name.clone(),
                credentials: live_creds,
                oauth_account: live_account,
            };
            save_profile(&self.accounts_dir, &profile)?;
            self.profiles.push(profile);
            self.profiles.sort_by(|a, b| a.name.cmp(&b.name));
            self.active = self.profiles.iter().position(|p| p.name == name);
        }
        Ok(())
    }

    // Swapped in profile become live cc account
    pub fn activate(&mut self, idx: usize) -> Result<()> {
        self.absorb_live()?; // capture freshest tokens of the outgoing account
        let p = &self.profiles[idx];
        write_live_creds(&self.creds_path, &p.credentials)?;
        if !p.oauth_account.is_null() {
            let mut claude_json = read_json(&self.claude_json_path).unwrap_or_else(|| json!({}));
            if let Some(obj) = claude_json.as_object_mut() {
                obj.insert("oauthAccount".to_string(), p.oauth_account.clone());
                write_atomic(
                    &self.claude_json_path,
                    serde_json::to_string(&claude_json)?.as_bytes(),
                )?;
            }
        }
        self.active = Some(idx);
        Ok(())
    }

    // Persist refreshed creds for tracked profile, ditch old tokens if there are any for live profile
    pub fn update_credentials(&mut self, name: &str, credentials: Value) -> Result<()> {
        let idx = match self.profiles.iter().position(|p| p.name == name) {
            Some(i) => i,
            None => return Ok(()),
        };
        self.profiles[idx].credentials = credentials;
        save_profile(&self.accounts_dir, &self.profiles[idx])?;
        if self.active == Some(idx) {
            write_live_creds(&self.creds_path, &self.profiles[idx].credentials)?;
        }
        Ok(())
    }

    pub fn delete(&mut self, name: &str) -> Result<()> {
        let idx = self
            .profiles
            .iter()
            .position(|p| p.name == name)
            .with_context(|| format!("no account named '{name}'"))?;
        if self.active == Some(idx) {
            bail!("'{name}' is the active account - switch to another first");
        }
        fs::remove_file(self.accounts_dir.join(format!("{name}.json")))?;
        self.profiles.remove(idx);
        self.active = self.active.map(|a| if a > idx { a - 1 } else { a });
        Ok(())
    }

    // Park live cc creds, run cc login, save res as profile
    pub fn add_account(&mut self, name: Option<&str>) -> Result<String> {
        self.absorb_live()?; // parked
        let backup = read_live_creds(&self.creds_path);
        clear_live_creds(&self.creds_path)?;
        let restore = |b: &Option<Value>| {
            if let Some(creds) = b {
                let _ = write_live_creds(&self.creds_path, creds);
            }
        };

        println!("\naccio starting `claude /login` - sign in then exit cc (ctrl+c)\n");
        let status = Command::new("claude").arg("/login").status();
        match status {
            Err(e) => {
                restore(&backup);
                return Err(e).context("could not run `claude` - is claude code on your PATH??");
            }
            Ok(_) => {}
        }

        let new_creds = match read_live_creds(&self.creds_path) {
            Some(v) => v,
            None => {
                restore(&backup);
                bail!("login was not completed - previous creds restored");
            }
        };
        let new_account = read_json(&self.claude_json_path)
            .and_then(|v| v.get("oauthAccount").cloned())
            .unwrap_or(Value::Null);
        let new_uuid = new_account.get("accountUuid").and_then(Value::as_str).map(str::to_string);

        if let Some(idx) = self
            .profiles
            .iter()
            .position(|p| new_uuid.is_some() && p.account_uuid() == new_uuid.as_deref())
        {
            self.profiles[idx].credentials = new_creds;
            self.profiles[idx].oauth_account = new_account;
            save_profile(&self.accounts_dir, &self.profiles[idx])?;
            self.active = Some(idx);
            return Ok(format!(
                "that account was already tracked as '{}' - credentials updated",
                self.profiles[idx].name
            ));
        }

        let email = new_account
            .get("emailAddress")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let base = match name.filter(|n| !n.trim().is_empty()) {
            Some(n) => sanitize_name(n),
            None => sanitize_name(email.split('@').next().unwrap_or("account")),
        };
        let name = self.unique_name(&base);
        let profile = Profile {
            name: name.clone(),
            credentials: new_creds,
            oauth_account: new_account,
        };
        save_profile(&self.accounts_dir, &profile)?;
        self.profiles.push(profile);
        self.profiles.sort_by(|a, b| a.name.cmp(&b.name));
        self.active = self.profiles.iter().position(|p| p.name == name);
        Ok(format!("added '{name}' ({email})"))
    }

    fn unique_name(&self, base: &str) -> String {
        let base = if base.is_empty() { "account" } else { base };
        if !self.profiles.iter().any(|p| p.name == base) {
            return base.to_string();
        }
        (2..)
            .map(|i| format!("{base}-{i}"))
            .find(|n| !self.profiles.iter().any(|p| p.name == *n))
            .unwrap()
    }
}

fn save_profile(dir: &Path, p: &Profile) -> Result<()> {
    let body = json!({
        "credentials": p.credentials,
        "oauth_account": p.oauth_account,
    });
    write_atomic(
        &dir.join(format!("{}.json", p.name)),
        serde_json::to_string_pretty(&body)?.as_bytes(),
    )
}

fn sanitize_name(s: &str) -> String {
    s.trim()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c.to_ascii_lowercase() } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

// Live cc creds sit in the keychain on mac
fn read_live_creds(creds_path: &Path) -> Option<Value> {
    #[cfg(target_os = "macos")]
    if let Some(v) = keychain_read() {
        return Some(v);
    }
    read_json(creds_path)
}

fn write_live_creds(creds_path: &Path, creds: &Value) -> Result<()> {
    let body = serde_json::to_string(creds)?;
    #[cfg(target_os = "macos")]
    if keychain_write(&body).is_ok() {
        // Stale file must not shadow the keychain
        let _ = fs::remove_file(creds_path);
        return Ok(());
    }
    write_atomic(creds_path, body.as_bytes())
}

fn clear_live_creds(creds_path: &Path) -> Result<()> {
    #[cfg(target_os = "macos")]
    let _ = Command::new("security")
        .args(["delete-generic-password", "-s", KEYCHAIN_SERVICE])
        .output();
    if creds_path.exists() {
        fs::remove_file(creds_path)?;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn keychain_read() -> Option<Value> {
    let out = Command::new("security")
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
    let out = Command::new("security")
        .args(["add-generic-password", "-U", "-a", &user, "-s", KEYCHAIN_SERVICE, "-w", body])
        .output()
        .context("cant run `security`")?;
    if !out.status.success() {
        bail!("keychain write failed - is the login keychain unlocked?");
    }
    Ok(())
}

// write tmp file in same dir w/ chmod 0600
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("accio-tmp");
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("cant write {}", tmp.display()))?;
        f.write_all(bytes)?;
        f.flush()?;
    }
    fs::rename(&tmp, path).with_context(|| format!("cant replace {}", path.display()))?;
    Ok(())
}
