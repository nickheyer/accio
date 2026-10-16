//! claude provider - each account keeps its credentials in one directory every claude process shares

mod oauth;
mod session;
mod store;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use accio_provider::{
    contract, files, parse_usage, read_json, write_atomic, Backend, Job, Knob, Outcome, Provider,
    Swap,
};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use store::{has_login, Slot};

const POINTER: &str = "CLAUDE_SECURESTORAGE_CONFIG_DIR";

pub fn provider() -> Result<impl Provider> {
    migrate_flat_profiles();
    Swap::load(Claude::locate()?)
}

pub fn session_provider() -> Result<impl Provider> {
    Swap::load_saved(Claude::locate()?)
}

struct Claude {
    live: Slot,
    claude_json_path: PathBuf,
    settings_path: PathBuf,
    settings_key: String,
    stores: PathBuf,
    sessions: PathBuf,
}

impl Claude {
    fn locate() -> Result<Self> {
        let home = dirs::home_dir().context("cant determine home dir")?;
        let config = dirs::config_dir()
            .context("cant determine config dir")?
            .join("accio");
        let (claude_dir, claude_json_path, settings_key, moved) =
            match std::env::var_os("CLAUDE_CONFIG_DIR") {
                Some(dir) => {
                    let dir = PathBuf::from(dir);
                    let json = dir.join(".claude.json");
                    let settings = dir.join("settings.json").display().to_string();
                    (dir, json, settings, true)
                }
                None => (
                    home.join(".claude"),
                    home.join(".claude.json"),
                    "~/.claude/settings.json".into(),
                    false,
                ),
            };
        Ok(Claude {
            live: Slot::live(&claude_dir, moved),
            claude_json_path,
            settings_path: claude_dir.join("settings.json"),
            settings_key,
            stores: config.join("accounts").join("claude"),
            sessions: config.join("sessions").join("claude"),
        })
    }

    // Directory the live settings send claude code to for credentials
    fn pointer(&self) -> Option<PathBuf> {
        read_json(&self.settings_path)?
            .pointer(&format!("/env/{POINTER}"))?
            .as_str()
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
    }

    fn set_pointer(&self, dir: Option<&Path>) -> Result<()> {
        let mut settings = read_json(&self.settings_path).unwrap_or_else(|| json!({}));
        let before = settings.clone();
        let obj = settings
            .as_object_mut()
            .with_context(|| format!("{} is not an object", self.settings_path.display()))?;
        match dir {
            Some(dir) => {
                let path = dir.to_str().context("account directory must be UTF-8")?;
                let env = obj.entry("env").or_insert_with(|| json!({}));
                if !env.is_object() {
                    *env = json!({});
                }
                env[POINTER] = Value::from(path);
            }
            None => {
                if let Some(env) = obj.get_mut("env").and_then(Value::as_object_mut) {
                    env.remove(POINTER);
                    if env.is_empty() {
                        obj.remove("env");
                    }
                }
            }
        }
        if settings == before {
            return Ok(());
        }
        if let Some(parent) = self.settings_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("cant create {}", parent.display()))?;
        }
        write_atomic(
            &self.settings_path,
            serde_json::to_string_pretty(&settings)?.as_bytes(),
        )
    }

    // Credentials older accio versions copied into session directories for this account
    fn left_behind(&self, oauth_account: Option<&String>) -> Vec<Value> {
        let Some(identity) = oauth_account
            .and_then(|a| serde_json::from_str::<Value>(a).ok())
            .map(|a| account_identity(&a))
            .filter(|i| !i.is_empty())
        else {
            return Vec::new();
        };
        let Ok(entries) = fs::read_dir(&self.sessions) else {
            return Vec::new();
        };
        entries
            .flatten()
            .map(|e| e.path())
            .filter(|dir| {
                read_json(&dir.join(".claude.json"))
                    .and_then(|v| v.get("oauthAccount").cloned())
                    .is_some_and(|a| account_identity(&a).iter().any(|i| identity.contains(i)))
            })
            .filter_map(|dir| read_json(&dir.join(".credentials.json")))
            .filter(has_login)
            .collect()
    }
}

// Email and account id of a claude.json oauthAccount
fn account_identity(account: &Value) -> Vec<String> {
    ["emailAddress", "accountUuid"]
        .iter()
        .filter_map(|k| account.get(k).and_then(Value::as_str))
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

// The profile entry naming an account directory, with the plan its credentials carry
fn store_entry(dir: &Path, creds: Option<&Value>) -> String {
    let mut entry = json!({ "dir": dir.to_string_lossy() });
    if let Some(plan) = creds
        .and_then(|c| c.pointer("/claudeAiOauth/subscriptionType"))
        .and_then(Value::as_str)
    {
        entry["subscriptionType"] = Value::from(plan);
    }
    entry.to_string()
}

fn entry_dir(entry: &str) -> Option<PathBuf> {
    serde_json::from_str::<Value>(entry)
        .ok()?
        .get("dir")?
        .as_str()
        .map(PathBuf::from)
}

fn store_dir(files: &BTreeMap<String, String>) -> Option<PathBuf> {
    files.get("credentials_dir").and_then(|e| entry_dir(e))
}

impl Backend for Claude {
    fn name(&self) -> &str {
        "claude"
    }

    fn read_live(&self) -> BTreeMap<String, String> {
        let mut live = BTreeMap::new();
        match self.pointer() {
            Some(dir) => match Slot::store(&dir).read() {
                Some(creds) if has_login(&creds) => {
                    live.insert(
                        "credentials_dir".to_string(),
                        store_entry(&dir, Some(&creds)),
                    );
                }
                _ => return live,
            },
            None => match self.live.read() {
                Some(creds) => {
                    live.insert("credentials".to_string(), creds.to_string());
                }
                None => return live,
            },
        }
        if let Some(acct) = read_json(&self.claude_json_path)
            .and_then(|v| v.get("oauthAccount").cloned())
            .filter(|a| !a.is_null())
        {
            live.insert("oauth_account".to_string(), acct.to_string());
        }
        live
    }

    // The pointer and account identity go live, credentials only ever sit in account directories
    fn write_live(&self, contents: &BTreeMap<String, String>) -> Result<()> {
        if contents.contains_key("credentials") {
            bail!("claude credentials belong in an account directory, not the live config");
        }
        let dir = match contents.get("credentials_dir") {
            Some(entry) => Some(entry_dir(entry).context("profile names no account directory")?),
            None => None,
        };
        self.set_pointer(dir.as_deref())?;
        self.live.clear()?;
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

    fn info(&self) -> Vec<(String, String)> {
        vec![
            (
                "credentials".to_string(),
                self.pointer()
                    .map(|d| contract(&d))
                    .unwrap_or_else(|| "-".to_string()),
            ),
            ("live".to_string(), contract(&self.live.path())),
            ("identity".to_string(), contract(&self.claude_json_path)),
            ("settings".to_string(), contract(&self.settings_path)),
            ("sessions".to_string(), contract(&self.sessions)),
            ("login".to_string(), self.login().join(" ")),
        ]
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
        fetch(files)
    }

    fn fetch_read_only(&self, files: BTreeMap<String, String>) -> Job {
        fetch(files)
    }

    fn session(
        &self,
        files: &BTreeMap<String, String>,
        dir: &Path,
    ) -> Result<std::process::Command> {
        session::prepare(self, files, dir)
    }

    fn is_login(&self, plain: &BTreeMap<String, String>) -> bool {
        let stored = store_dir(plain)
            .and_then(|d| Slot::store(&d).read())
            .is_some_and(|c| has_login(&c));
        stored
            || plain
                .get("credentials")
                .and_then(|s| serde_json::from_str::<Value>(s).ok())
                .is_some_and(|c| has_login(&c))
    }

    fn owned(&self) -> BTreeMap<String, String> {
        self.compose(
            &self
                .knobs()
                .into_iter()
                .map(|k| (k.name, String::new()))
                .collect(),
        )
    }

    // Copied credentials move into the profile's own directory, the newest chain of each account wins
    fn adopt(&self, name: &str, files: &mut BTreeMap<String, String>) -> Result<bool> {
        let own = self.stores.join(name);
        let slot = Slot::store(&own);
        let mut changed = false;
        if let Some(snapshot) = files.remove("credentials") {
            changed = true;
            if let Ok(creds) = serde_json::from_str::<Value>(&snapshot) {
                slot.offer(&creds)?;
            }
            for creds in self.left_behind(files.get("oauth_account")) {
                slot.offer(&creds)?;
            }
        }
        if let Some(dir) = store_dir(files) {
            if dir != own && dir.starts_with(&self.stores) {
                if let Some(creds) = Slot::store(&dir).take()? {
                    slot.offer(&creds)?;
                }
                files.remove("credentials_dir");
                changed = true;
            }
        }
        let dir = store_dir(files).unwrap_or(own);
        let creds = Slot::store(&dir).read().filter(has_login);
        if changed || files.contains_key("credentials_dir") || creds.is_some() {
            let entry = store_entry(&dir, creds.as_ref());
            if files.get("credentials_dir") != Some(&entry) {
                files.insert("credentials_dir".to_string(), entry);
                changed = true;
            }
        }
        Ok(changed)
    }

    fn forget(&self, name: &str) -> Result<()> {
        let dir = self.stores.join(name);
        Slot::store(&dir).clear()?;
        if dir.exists() {
            fs::remove_dir_all(&dir).with_context(|| format!("cant remove {}", dir.display()))?;
        }
        Ok(())
    }
}

fn fetch(files: BTreeMap<String, String>) -> Job {
    Box::new(move || {
        let Some(dir) = store_dir(&files) else {
            if files.contains_key("oauth_account") {
                return Outcome {
                    usage: Err("no credentials - add this account again".to_string()),
                    state: None,
                };
            }
            // configured profiles have no oauth, show their facts instead
            return files::facts_job(files)();
        };
        let token = match Slot::store(&dir).fresh_token() {
            Ok(t) => t,
            Err(e) => {
                return Outcome {
                    usage: Err(format!("{e:#}")),
                    state: None,
                }
            }
        };
        let usage = match oauth::fetch_usage(&token) {
            Ok(v) => {
                let u = parse_usage(&v);
                if u.metrics.is_empty() {
                    Err("no usage data in response".to_string())
                } else {
                    Ok(u)
                }
            }
            Err(e) => Err(format!("{e:#}")),
        };
        Outcome { usage, state: None }
    })
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
        if !path.is_file() || path.extension().is_none_or(|e| e != "json") {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn root(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("accio-claude-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(d.join("live")).unwrap();
        d
    }

    fn claude(root: &Path) -> Claude {
        Claude {
            live: Slot::live(&root.join("live"), true),
            claude_json_path: root.join("live/.claude.json"),
            settings_path: root.join("live/settings.json"),
            settings_key: root.join("live/settings.json").display().to_string(),
            stores: root.join("accounts"),
            sessions: root.join("sessions"),
        }
    }

    fn creds(expires_at: i64) -> Value {
        json!({"claudeAiOauth": {"accessToken": format!("a{expires_at}"), "refreshToken": "r",
            "expiresAt": expires_at, "subscriptionType": "max"}})
    }

    fn account(email: &str) -> String {
        json!({"emailAddress": email, "accountUuid": format!("uuid-{email}")}).to_string()
    }

    fn plain(creds: &str) -> BTreeMap<String, String> {
        BTreeMap::from([("credentials".to_string(), creds.to_string())])
    }

    #[test]
    fn login_needs_a_claude_ai_token() {
        let r = root("login");
        let c = claude(&r);
        assert!(!c.is_login(&plain(
            r#"{"mcpOAuth":{"plugin:x|1":{"accessToken":"","clientId":"c"}}}"#
        )));
        assert!(!c.is_login(&plain(
            r#"{"claudeAiOauth":{"accessToken":"","refreshToken":"","expiresAt":0}}"#
        )));
        assert!(!c.is_login(&plain("not json")));
        assert!(!c.is_login(&BTreeMap::from([(
            "oauth_account".to_string(),
            "{}".to_string()
        )])));
        assert!(c.is_login(&plain(
            r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat01-x"}}"#
        )));
        assert!(c.is_login(&plain(
            r#"{"claudeAiOauth":{"accessToken":"","refreshToken":"sk-ant-ort01-x"}}"#
        )));
        let dir = r.join("accounts/nick");
        let stored = BTreeMap::from([("credentials_dir".to_string(), store_entry(&dir, None))]);
        assert!(!c.is_login(&stored));
        Slot::store(&dir).write(&creds(5)).unwrap();
        assert!(c.is_login(&stored));
        Slot::store(&dir)
            .write(&json!({"claudeAiOauth": {"accessToken": "", "refreshToken": ""}}))
            .unwrap();
        assert!(!c.is_login(&stored));
    }

    #[test]
    fn owned_claims_every_knob_in_the_settings_env() {
        let r = root("owned");
        let c = claude(&r);
        let owned = c.owned();
        let key = format!("merge:{}", r.join("live/settings.json").display());
        let doc: Value = serde_json::from_str(&owned[&key]).unwrap();
        let mut keys: Vec<String> = doc["env"].as_object().unwrap().keys().cloned().collect();
        keys.sort();
        let mut knobs: Vec<String> = c.knobs().into_iter().map(|k| k.name).collect();
        knobs.sort();
        assert_eq!(keys, knobs);
        assert!(doc["env"].as_object().unwrap().values().all(|v| v == ""));
    }

    #[test]
    fn adopt_moves_a_snapshot_into_the_store_keeping_the_newest_chain() {
        let r = root("adopt");
        let c = claude(&r);
        for (i, (expires, email)) in [(2, "nick@x.co"), (9, "other@x.co"), (3, "nick@x.co")]
            .iter()
            .enumerate()
        {
            let dir = r.join(format!("sessions/{i}"));
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join(".credentials.json"), creds(*expires).to_string()).unwrap();
            fs::write(
                dir.join(".claude.json"),
                json!({"oauthAccount": {"emailAddress": email}}).to_string(),
            )
            .unwrap();
        }
        let mut files = BTreeMap::from([
            ("credentials".to_string(), creds(1).to_string()),
            ("oauth_account".to_string(), account("nick@x.co")),
        ]);
        assert!(c.adopt("nick", &mut files).unwrap());
        let dir = r.join("accounts/nick");
        assert_eq!(
            files,
            BTreeMap::from([
                (
                    "credentials_dir".to_string(),
                    store_entry(&dir, Some(&creds(3)))
                ),
                ("oauth_account".to_string(), account("nick@x.co")),
            ])
        );
        assert_eq!(store::expires_at(&Slot::store(&dir).read().unwrap()), 3);
        assert!(!c.adopt("nick", &mut files).unwrap());
        assert!(files["credentials_dir"].contains("\"subscriptionType\":\"max\""));

        let mut dead = BTreeMap::from([(
            "credentials".to_string(),
            r#"{"claudeAiOauth":{"accessToken":"","refreshToken":""}}"#.to_string(),
        )]);
        assert!(c.adopt("dead", &mut dead).unwrap());
        assert_eq!(
            dead,
            BTreeMap::from([(
                "credentials_dir".to_string(),
                store_entry(&r.join("accounts/dead"), None)
            )])
        );
        assert!(!c.is_login(&dead));
        let mut configured = BTreeMap::from([("merge:x".to_string(), "{}".to_string())]);
        assert!(!c.adopt("glm", &mut configured).unwrap());
        assert_eq!(configured.len(), 1);
    }

    #[test]
    fn adopt_reclaims_a_login_that_landed_in_another_profiles_store() {
        let r = root("reclaim");
        let c = claude(&r);
        let x = r.join("accounts/x");
        Slot::store(&x).write(&creds(7)).unwrap();
        let mut files = BTreeMap::from([
            (
                "credentials_dir".to_string(),
                store_entry(&x, Some(&creds(7))),
            ),
            ("oauth_account".to_string(), account("y@x.co")),
        ]);
        assert!(c.adopt("y", &mut files).unwrap());
        let y = r.join("accounts/y");
        assert_eq!(files["credentials_dir"], store_entry(&y, Some(&creds(7))));
        assert_eq!(store::expires_at(&Slot::store(&y).read().unwrap()), 7);
        assert!(Slot::store(&x).read().is_none());

        let foreign = r.join("elsewhere");
        Slot::store(&foreign).write(&creds(8)).unwrap();
        let mut files =
            BTreeMap::from([("credentials_dir".to_string(), store_entry(&foreign, None))]);
        assert!(c.adopt("z", &mut files).unwrap());
        assert_eq!(
            files["credentials_dir"],
            store_entry(&foreign, Some(&creds(8)))
        );
        assert!(Slot::store(&foreign).read().is_some());
        assert!(!r.join("accounts/z").exists());
        c.forget("y").unwrap();
        assert!(!y.exists());
    }

    #[test]
    fn live_surface_is_the_pointer_and_a_login_without_one_is_a_snapshot() {
        let r = root("live");
        let c = claude(&r);
        assert!(c.read_live().is_empty());
        fs::write(
            r.join("live/.claude.json"),
            json!({"hasCompletedOnboarding": true, "oauthAccount": {"emailAddress": "nick@x.co"}})
                .to_string(),
        )
        .unwrap();
        c.live.write(&creds(4)).unwrap();
        let live = c.read_live();
        assert_eq!(live["credentials"], creds(4).to_string());
        assert_eq!(live["oauth_account"], r#"{"emailAddress":"nick@x.co"}"#);
        assert!(c.write_live(&live).is_err());

        let dir = r.join("accounts/nick");
        Slot::store(&dir).write(&creds(4)).unwrap();
        let profile = BTreeMap::from([
            (
                "credentials_dir".to_string(),
                store_entry(&dir, Some(&creds(4))),
            ),
            ("oauth_account".to_string(), account("nick@x.co")),
        ]);
        fs::write(r.join("live/settings.json"), r#"{"theme":"dark"}"#).unwrap();
        c.write_live(&profile).unwrap();
        assert!(c.live.read().is_none());
        assert_eq!(c.pointer().unwrap(), dir);
        assert_eq!(
            read_json(&r.join("live/settings.json")).unwrap()["theme"],
            "dark"
        );
        assert_eq!(c.read_live(), profile);
        assert_eq!(
            read_json(&r.join("live/.claude.json")).unwrap()["oauthAccount"]["accountUuid"],
            "uuid-nick@x.co"
        );

        c.write_live(&BTreeMap::new()).unwrap();
        assert!(c.pointer().is_none());
        assert!(read_json(&r.join("live/settings.json"))
            .unwrap()
            .get("env")
            .is_none());
        assert!(read_json(&r.join("live/.claude.json"))
            .unwrap()
            .get("oauthAccount")
            .is_none());
        assert!(c.read_live().is_empty());
        assert!(
            Slot::store(&dir).read().is_some(),
            "stores outlive switches"
        );

        c.write_live(&profile).unwrap();
        Slot::store(&dir)
            .write(&json!({"claudeAiOauth": {"accessToken": "", "refreshToken": ""}}))
            .unwrap();
        assert!(c.read_live().is_empty(), "a logged out store is no login");
    }
}
