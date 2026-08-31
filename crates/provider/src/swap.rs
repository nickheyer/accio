//! A provider whose accounts are snapshots of its backend's live credentials

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde_json::{json, Map, Value};

use crate::fsutil::{read_json, sanitize_name, write_atomic};
use crate::{overlay, Account, Fetch, Job, Knob, Provider};

// the parts of a provider that are actually its own
pub trait Backend {
    fn name(&self) -> &str;
    fn read_live(&self) -> BTreeMap<String, String>;
    fn write_live(&self, contents: &BTreeMap<String, String>) -> Result<()>;
    fn login(&self) -> &[&str];
    fn fetch(&self, files: BTreeMap<String, String>) -> Job;
    fn info(&self) -> Vec<(String, String)> {
        Vec::new()
    }

    // settings a configured profile exposes
    fn knobs(&self) -> Vec<Knob> {
        Vec::new()
    }

    // turn filled knobs into profile file entries
    fn compose(&self, _values: &BTreeMap<String, String>) -> BTreeMap<String, String> {
        BTreeMap::new()
    }
}

pub struct Swap<B: Backend> {
    backend: B,
    profiles: Vec<Profile>,
    active: Option<usize>,
    dir: PathBuf,
}

struct Profile {
    name: String,
    email: Option<String>,
    plan: Option<String>,
    identity: Vec<String>,
    files: BTreeMap<String, String>,
}

impl<B: Backend> Swap<B> {
    pub fn load(backend: B) -> Result<Self> {
        let dir = dirs::config_dir()
            .context("cant determine config dir")?
            .join("accio")
            .join("accounts")
            .join(backend.name());
        fs::create_dir_all(&dir).with_context(|| format!("cant create {}", dir.display()))?;
        let mut store = Swap { backend, profiles: Vec::new(), active: None, dir };
        for entry in fs::read_dir(&store.dir)? {
            let path = entry?.path();
            if path.extension().map_or(true, |e| e != "json") {
                continue;
            }
            let pname = match path.file_stem().and_then(|s| s.to_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            let v = match read_json(&path) {
                Some(v) => v,
                None => continue,
            };
            let contents: BTreeMap<String, String> = v
                .get("files")
                .and_then(Value::as_object)
                .map(|m| {
                    m.iter()
                        .filter_map(|(k, c)| c.as_str().map(|s| (k.clone(), s.to_string())))
                        .collect()
                })
                .unwrap_or_default();
            store.profiles.push(Profile::new(pname, contents));
        }
        store.profiles.sort_by(|a, b| a.name.cmp(&b.name));
        store.absorb_live()?;
        Ok(store)
    }

    // union of every profile's overlay claims, so switches clean up after each other
    fn spec(&self) -> overlay::Spec {
        let mut spec = overlay::Spec::default();
        for p in &self.profiles {
            spec.add(&p.files);
        }
        spec
    }

    fn read_surface(&self) -> BTreeMap<String, String> {
        let mut live = overlay::read(&self.spec());
        live.extend(self.backend.read_live());
        live
    }

    fn write_surface(&self, contents: &BTreeMap<String, String>) -> Result<()> {
        let (overlays, plain) = overlay::split(contents);
        self.backend.write_live(&plain)?;
        let mut spec = self.spec();
        spec.add(&overlays);
        overlay::write(&spec, &overlays)
    }

    fn absorb_live(&mut self) -> Result<()> {
        self.active = None;
        let live = self.read_surface();
        if live.is_empty() {
            return Ok(());
        }

        if let Some(idx) = self.profiles.iter().position(|p| p.files == live) {
            self.active = Some(idx);
            return self.set_marker(idx);
        }

        let incoming = Profile::new(String::new(), live);
        let by_identity = (!incoming.identity.is_empty())
            .then(|| {
                self.profiles
                    .iter()
                    .position(|p| shares_identity(&p.identity, &incoming.identity))
            })
            .flatten();
        // tokens rotate without telling us who they belong to - trust the marker then
        let by_marker = self
            .marker()
            .and_then(|m| self.profiles.iter().position(|p| p.name == m))
            .filter(|&i| incoming.identity.is_empty() || self.profiles[i].identity.is_empty());

        if let Some(idx) = by_identity.or(by_marker) {
            self.profiles[idx].files = incoming.files;
            self.profiles[idx].derive();
            save_profile(&self.dir, &self.profiles[idx])?;
            self.active = Some(idx);
            return self.set_marker(idx);
        }

        let base = incoming
            .email
            .as_deref()
            .map(|e| e.split('@').next().unwrap_or(e))
            .unwrap_or("account");
        let name = self.unique_name(&sanitize_name(base));
        let profile = Profile { name: name.clone(), ..incoming };
        save_profile(&self.dir, &profile)?;
        self.profiles.push(profile);
        self.profiles.sort_by(|a, b| a.name.cmp(&b.name));
        self.active = self.profiles.iter().position(|p| p.name == name);
        if let Some(idx) = self.active {
            self.set_marker(idx)?;
        }
        Ok(())
    }

    fn marker(&self) -> Option<String> {
        let s = fs::read_to_string(self.dir.join(".active")).ok()?;
        let s = s.trim().to_string();
        (!s.is_empty()).then_some(s)
    }

    fn set_marker(&self, idx: usize) -> Result<()> {
        let name = &self.profiles[idx].name;
        let path = self.dir.join(".active");
        if fs::read_to_string(&path).map(|s| s.trim() == name.as_str()).unwrap_or(false) {
            return Ok(());
        }
        write_atomic(&path, name.as_bytes())
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

impl<B: Backend> Provider for Swap<B> {
    fn name(&self) -> &str {
        self.backend.name()
    }

    fn accounts(&self) -> Vec<Account> {
        self.profiles
            .iter()
            .map(|p| Account {
                name: p.name.clone(),
                email: p.email.clone(),
                plan: p.plan.clone(),
            })
            .collect()
    }

    fn active(&self) -> Option<usize> {
        self.active
    }

    fn activate(&mut self, idx: usize) -> Result<()> {
        let name = self
            .profiles
            .get(idx)
            .map(|p| p.name.clone())
            .context("no such account")?;
        self.absorb_live()?; // capture freshest tokens of the outgoing account
        let idx = self
            .profiles
            .iter()
            .position(|p| p.name == name)
            .context("account vanished during switch")?;
        self.write_surface(&self.profiles[idx].files)?;
        self.active = Some(idx);
        self.set_marker(idx)
    }

    fn delete(&mut self, name: &str) -> Result<()> {
        let idx = self
            .profiles
            .iter()
            .position(|p| p.name == name)
            .with_context(|| format!("no account named '{name}'"))?;
        if self.active == Some(idx) {
            bail!("'{name}' is the active account - switch to another first");
        }
        fs::remove_file(self.dir.join(format!("{name}.json")))?;
        self.profiles.remove(idx);
        self.active = self.active.map(|a| if a > idx { a - 1 } else { a });
        Ok(())
    }

    fn add(&mut self, name: Option<&str>) -> Result<String> {
        let login: Vec<String> = self.backend.login().iter().map(|s| s.to_string()).collect();
        if login.is_empty() {
            bail!("no login command for '{}'", self.backend.name());
        }
        self.absorb_live()?; // parked
        let backup = self.read_surface();
        self.write_surface(&BTreeMap::new())?;

        println!("\naccio starting `{}` - sign in then exit\n", login.join(" "));
        let status = Command::new(&login[0]).args(&login[1..]).status();
        if let Err(e) = status {
            let _ = self.write_surface(&backup);
            return Err(e)
                .context(format!("could not run `{}` - is it on your PATH??", login[0]));
        }

        let live = self.read_surface();
        if live.is_empty() {
            let _ = self.write_surface(&backup);
            bail!("login was not completed - previous creds restored");
        }

        let incoming = Profile::new(String::new(), live);
        if let Some(idx) = (!incoming.identity.is_empty())
            .then(|| {
                self.profiles
                    .iter()
                    .position(|p| shares_identity(&p.identity, &incoming.identity))
            })
            .flatten()
        {
            self.profiles[idx].files = incoming.files;
            self.profiles[idx].derive();
            save_profile(&self.dir, &self.profiles[idx])?;
            self.active = Some(idx);
            self.set_marker(idx)?;
            return Ok(format!(
                "that account was already tracked as '{}' - credentials updated",
                self.profiles[idx].name
            ));
        }

        let email = incoming.email.clone().unwrap_or_else(|| "unknown".to_string());
        let base = match name.filter(|n| !n.trim().is_empty()) {
            Some(n) => sanitize_name(n),
            None => sanitize_name(email.split('@').next().unwrap_or("account")),
        };
        let name = self.unique_name(&base);
        let profile = Profile { name: name.clone(), ..incoming };
        save_profile(&self.dir, &profile)?;
        self.profiles.push(profile);
        self.profiles.sort_by(|a, b| a.name.cmp(&b.name));
        self.active = self.profiles.iter().position(|p| p.name == name);
        if let Some(idx) = self.active {
            self.set_marker(idx)?;
        }
        Ok(format!("added '{name}' ({email})"))
    }

    fn knobs(&self) -> Vec<Knob> {
        self.backend.knobs()
    }

    fn configure(&mut self, name: Option<&str>, values: &BTreeMap<String, String>) -> Result<String> {
        let values: BTreeMap<String, String> = values
            .iter()
            .filter(|(k, v)| !k.trim().is_empty() && !v.trim().is_empty())
            .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
            .collect();
        if values.is_empty() {
            bail!("nothing to configure");
        }
        let bag = self.backend.compose(&values);
        if bag.is_empty() {
            bail!("'{}' does not support configured profiles", self.backend.name());
        }
        let incoming = Profile::new(String::new(), bag);
        let base = name
            .filter(|n| !n.trim().is_empty())
            .map(sanitize_name)
            .or_else(|| incoming.email.as_deref().map(sanitize_name))
            .filter(|b| !b.is_empty())
            .unwrap_or_else(|| "custom".to_string());
        let name = self.unique_name(&base);
        let profile = Profile { name: name.clone(), ..incoming };
        save_profile(&self.dir, &profile)?;
        let active_name = self.active.map(|i| self.profiles[i].name.clone());
        self.profiles.push(profile);
        self.profiles.sort_by(|a, b| a.name.cmp(&b.name));
        self.active =
            active_name.and_then(|n| self.profiles.iter().position(|p| p.name == n));
        Ok(format!("configured '{name}'"))
    }

    fn refresh(&mut self) -> Result<()> {
        self.absorb_live()
    }

    fn fetches(&self) -> Vec<Fetch> {
        self.profiles
            .iter()
            .map(|p| Fetch { account: p.name.clone(), job: self.backend.fetch(p.files.clone()) })
            .collect()
    }

    fn absorb_fetch(&mut self, account: &str, state: Value) -> Result<()> {
        let idx = match self.profiles.iter().position(|p| p.name == account) {
            Some(i) => i,
            None => return Ok(()),
        };
        let entries = match state.as_object() {
            Some(m) => m,
            None => return Ok(()),
        };
        for (k, v) in entries {
            if let Some(s) = v.as_str() {
                self.profiles[idx].files.insert(k.clone(), s.to_string());
            }
        }
        self.profiles[idx].derive();
        save_profile(&self.dir, &self.profiles[idx])?;
        if self.active == Some(idx) {
            self.write_surface(&self.profiles[idx].files)?;
        }
        Ok(())
    }

    fn info(&self) -> Vec<(String, String)> {
        self.backend.info()
    }
}

impl Profile {
    fn new(name: String, files: BTreeMap<String, String>) -> Self {
        let files = files
            .into_iter()
            .map(|(k, v)| {
                let v = if overlay::is_merge_key(&k) { overlay::canon_str(&v) } else { v };
                (k, v)
            })
            .collect();
        let mut p = Profile { name, email: None, plan: None, identity: Vec::new(), files };
        p.derive();
        p
    }

    // whatever the files say about who this is - emails and account ids, JWT payloads included
    fn derive(&mut self) {
        let (mut emails, mut ids, mut plan) = (Vec::new(), Vec::new(), None);
        let mut endpoint = None;
        for content in self.files.values() {
            let v: Value = match serde_json::from_str(content) {
                Ok(v) => v,
                Err(_) => match dotenv_json(content) {
                    Some(v) => v,
                    None => continue,
                },
            };
            walk_values(&v, &mut |key: &str, val: &Value| {
                let Some(s) = val.as_str() else { return };
                if looks_like_email(s) {
                    emails.push(s.to_string());
                } else if key.contains("account") && (key.contains("id") || key.contains("uuid")) {
                    ids.push(s.to_string());
                }
                if plan.is_none()
                    && !s.is_empty()
                    && ["plan", "tier", "subscription"].iter().any(|w| key.contains(w))
                {
                    plan = Some(s.to_string());
                }
                if endpoint.is_none() && (key.contains("base_url") || key.contains("baseurl")) {
                    endpoint = url_host(s);
                }
            });
        }
        emails.sort();
        emails.dedup();
        self.email = emails.first().cloned().or(endpoint);
        self.plan = plan;
        let mut identity = emails;
        identity.extend(ids);
        identity.sort();
        identity.dedup();
        self.identity = identity;
    }
}

// KEY=VALUE lines as an object so derive can look inside env files
fn dotenv_json(content: &str) -> Option<Value> {
    let mut map = Map::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (k, v) = line.split_once('=')?;
        map.insert(k.trim().to_string(), Value::from(v.trim()));
    }
    (!map.is_empty()).then(|| Value::Object(map))
}

fn url_host(s: &str) -> Option<String> {
    let rest = s.strip_prefix("https://").or_else(|| s.strip_prefix("http://"))?;
    let host = rest.split(['/', ':']).next().unwrap_or("");
    (!host.is_empty()).then(|| host.to_string())
}

fn save_profile(dir: &Path, p: &Profile) -> Result<()> {
    let files: Map<String, Value> =
        p.files.iter().map(|(k, v)| (k.clone(), Value::from(v.as_str()))).collect();
    write_atomic(
        &dir.join(format!("{}.json", p.name)),
        serde_json::to_string_pretty(&json!({ "files": files }))?.as_bytes(),
    )
}

fn shares_identity(a: &[String], b: &[String]) -> bool {
    a.iter().any(|x| b.contains(x))
}

// every (lowercased key, value) pair, descending into JWT payloads where a value holds one
fn walk_values(v: &Value, f: &mut impl FnMut(&str, &Value)) {
    match v {
        Value::Object(map) => {
            for (k, child) in map {
                f(&k.to_ascii_lowercase(), child);
                if let Some(payload) = child.as_str().and_then(jwt_payload) {
                    walk_values(&payload, f);
                } else {
                    walk_values(child, f);
                }
            }
        }
        Value::Array(items) => items.iter().for_each(|i| walk_values(i, f)),
        _ => {}
    }
}

fn jwt_payload(s: &str) -> Option<Value> {
    if !s.starts_with("eyJ") {
        return None;
    }
    let mut parts = s.split('.');
    let payload = match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(_), Some(p), Some(_), None) => p,
        _ => return None,
    };
    serde_json::from_slice(&b64url_decode(payload)?).ok()
}

fn b64url_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for b in s.bytes() {
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            b'=' => break,
            _ => return None,
        } as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

fn looks_like_email(s: &str) -> bool {
    let Some((user, host)) = s.split_once('@') else { return false };
    !user.is_empty()
        && !host.contains('@')
        && host.contains('.')
        && !host.ends_with('.')
        && !s.chars().any(|c| c.is_whitespace() || c == '/' || c == ':' || c == '<' || c == '>')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn email_shapes() {
        assert!(looks_like_email("a@b.co"));
        assert!(looks_like_email("first.last@sub.example.com"));
        assert!(!looks_like_email("https://x.y/z@w.co"));
        assert!(!looks_like_email("no-at.example.com"));
        assert!(!looks_like_email("a@b"));
        assert!(!looks_like_email("a b@c.d"));
    }

    #[test]
    fn derives_identity_from_jwt() {
        // payload is {"email":"x@y.z"}
        let token = "eyJhbGciOiJub25lIn0.eyJlbWFpbCI6InhAeS56In0.sig";
        let mut files = BTreeMap::new();
        files.insert(
            "~/.codex/auth.json".to_string(),
            format!(r#"{{"tokens":{{"id_token":"{token}","account_id":"acc-1"}}}}"#),
        );
        let p = Profile::new("t".into(), files);
        assert_eq!(p.email.as_deref(), Some("x@y.z"));
        assert!(p.identity.contains(&"x@y.z".to_string()));
        assert!(p.identity.contains(&"acc-1".to_string()));
    }

    #[test]
    fn derives_plan_and_plain_email() {
        let mut files = BTreeMap::new();
        files.insert(
            "~/.x/creds.json".to_string(),
            r#"{"active": "me@mail.com", "chatgpt_plan_type": "plus"}"#.to_string(),
        );
        let p = Profile::new("t".into(), files);
        assert_eq!(p.email.as_deref(), Some("me@mail.com"));
        assert_eq!(p.plan.as_deref(), Some("plus"));
    }

    #[test]
    fn derives_endpoint_host_for_configured_profiles() {
        let mut files = BTreeMap::new();
        files.insert(
            "merge:~/.claude/settings.json".to_string(),
            r#"{"env":{"ANTHROPIC_BASE_URL":"https://api.z.ai/api/anthropic","ANTHROPIC_AUTH_TOKEN":"sk-x"}}"#
                .to_string(),
        );
        let p = Profile::new("t".into(), files);
        assert_eq!(p.email.as_deref(), Some("api.z.ai"));
        assert!(p.identity.is_empty());
    }

    #[test]
    fn derives_from_dotenv_files() {
        let mut files = BTreeMap::new();
        files.insert(
            "~/.gemini/.env".to_string(),
            "GOOGLE_GEMINI_BASE_URL=https://g.example.com:8080/v1\nGEMINI_API_KEY=k\n".to_string(),
        );
        let p = Profile::new("t".into(), files);
        assert_eq!(p.email.as_deref(), Some("g.example.com"));
    }

    #[test]
    fn merge_values_are_canonicalized() {
        let mut files = BTreeMap::new();
        files.insert("merge:~/x.json".to_string(), r#"{"b":1,"a":2}"#.to_string());
        let p = Profile::new("t".into(), files);
        assert_eq!(p.files["merge:~/x.json"], r#"{"a":2,"b":1}"#);
    }

    #[test]
    fn identity_overlap_not_exact_match() {
        assert!(shares_identity(
            &["a@b.co".into(), "acc-1".into()],
            &["acc-1".into()]
        ));
        assert!(!shares_identity(&["a@b.co".into()], &["c@d.co".into()]));
        assert!(!shares_identity(&[], &[]));
    }
}
