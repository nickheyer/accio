//! One account's credentials where claude code keeps them, shared by every process using that account

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use accio_provider::{read_json, write_atomic};
use anyhow::{bail, Context, Result};
use chrono::Utc;
use serde_json::Value;

const FILE: &str = ".credentials.json";
const STALE: Duration = Duration::from_secs(60);
const WAIT: Duration = Duration::from_secs(20);

pub struct Slot {
    dir: PathBuf,
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    hashed: bool,
}

// Claude code names its lock directories after the store, abandoned ones age out after a minute
#[derive(Debug)]
pub struct Lock {
    held: Vec<PathBuf>,
}

impl Drop for Lock {
    fn drop(&mut self) {
        for path in self.held.iter().rev() {
            let _ = fs::remove_dir(path);
        }
    }
}

impl Slot {
    // The default claude directory, keyed by its path once CLAUDE_CONFIG_DIR moves it
    pub fn live(dir: &Path, moved: bool) -> Slot {
        Slot {
            dir: dir.to_path_buf(),
            hashed: moved,
        }
    }

    pub fn store(dir: &Path) -> Slot {
        Slot {
            dir: dir.to_path_buf(),
            hashed: true,
        }
    }

    pub fn path(&self) -> PathBuf {
        self.dir.join(FILE)
    }

    pub fn read(&self) -> Option<Value> {
        #[cfg(target_os = "macos")]
        if let Some(v) = keychain::read(&self.service()) {
            return Some(v);
        }
        read_json(&self.path())
    }

    pub fn write(&self, creds: &Value) -> Result<()> {
        self.create_dir()?;
        let body = creds.to_string();
        #[cfg(target_os = "macos")]
        if keychain::write(&self.service(), &body).is_ok() {
            let _ = fs::remove_file(self.path());
            return Ok(());
        }
        write_atomic(&self.path(), body.as_bytes())
    }

    pub fn clear(&self) -> Result<()> {
        #[cfg(target_os = "macos")]
        keychain::delete(&self.service());
        let path = self.path();
        if path.exists() {
            fs::remove_file(&path).with_context(|| format!("cant remove {}", path.display()))?;
        }
        Ok(())
    }

    pub fn lock(&self) -> Result<Lock> {
        self.lock_within(WAIT)
    }

    // Newer credentials replace what is held, older ones are dropped
    pub fn offer(&self, candidate: &Value) -> Result<bool> {
        if !has_login(candidate) {
            return Ok(false);
        }
        let _lock = self.lock()?;
        let held = self.read();
        if held
            .as_ref()
            .is_some_and(|h| has_login(h) && expires_at(h) >= expires_at(candidate))
        {
            return Ok(false);
        }
        self.write(candidate)?;
        Ok(true)
    }

    pub fn take(&self) -> Result<Option<Value>> {
        if self.read().is_none() {
            return Ok(None);
        }
        let _lock = self.lock()?;
        let held = self.read();
        if held.is_some() {
            self.clear()?;
        }
        Ok(held)
    }

    // Access token good for two more minutes, refreshed under claude code's own lock otherwise
    pub fn fresh_token(&self) -> Result<String> {
        let creds = self
            .read()
            .context("no credentials - add this account again")?;
        if fresh(&creds) {
            return token(&creds);
        }
        let _lock = self.lock()?;
        let mut creds = self
            .read()
            .context("no credentials - add this account again")?;
        if fresh(&creds) {
            return token(&creds);
        }
        crate::oauth::refresh(&mut creds)?;
        self.write(&creds)?;
        token(&creds)
    }

    fn lock_within(&self, wait: Duration) -> Result<Lock> {
        self.create_dir()?;
        let legacy = fs::canonicalize(self.path()).unwrap_or_else(|_| self.path());
        let mut lock = Lock { held: Vec::new() };
        for target in [
            self.dir.join(".oauth_refresh.lock"),
            PathBuf::from(format!("{}.lock", legacy.display())),
        ] {
            acquire(&target, wait)?;
            lock.held.push(target);
        }
        Ok(lock)
    }

    fn create_dir(&self) -> Result<()> {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.dir)
            .with_context(|| format!("cant create {}", self.dir.display()))
    }

    #[cfg(target_os = "macos")]
    fn service(&self) -> String {
        use sha2::{Digest, Sha256};
        use unicode_normalization::UnicodeNormalization;
        let base = "Claude Code-credentials";
        if !self.hashed {
            return base.to_string();
        }
        let nfc: String = self.dir.to_string_lossy().nfc().collect();
        let hex = format!("{:x}", Sha256::digest(nfc.as_bytes()));
        format!("{base}-{}", &hex[..8])
    }
}

fn acquire(target: &Path, wait: Duration) -> Result<()> {
    let start = Instant::now();
    loop {
        match fs::create_dir(target) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e).with_context(|| format!("cant lock {}", target.display())),
        }
        let age = fs::metadata(target)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| SystemTime::now().duration_since(t).ok());
        match age {
            None => continue,
            Some(age) if age > STALE => {
                let _ = fs::remove_dir(target);
            }
            Some(_) if start.elapsed() > wait => {
                bail!("another claude process is refreshing this account - try again shortly")
            }
            Some(_) => std::thread::sleep(Duration::from_millis(250)),
        }
    }
}

// A claude.ai access or refresh token makes a login, mcp discovery state alone does not
pub fn has_login(creds: &Value) -> bool {
    ["accessToken", "refreshToken"].iter().any(|k| {
        creds
            .pointer(&format!("/claudeAiOauth/{k}"))
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty())
    })
}

pub fn expires_at(creds: &Value) -> i64 {
    creds
        .pointer("/claudeAiOauth/expiresAt")
        .and_then(Value::as_i64)
        .unwrap_or(0)
}

fn fresh(creds: &Value) -> bool {
    expires_at(creds) - Utc::now().timestamp_millis() > 120_000
}

fn token(creds: &Value) -> Result<String> {
    creds
        .pointer("/claudeAiOauth/accessToken")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .context("no access token - add this account again")
}

#[cfg(target_os = "macos")]
mod keychain {
    use anyhow::{Context, Result};
    use serde_json::Value;
    use std::process::Command;

    pub fn read(service: &str) -> Option<Value> {
        let out = Command::new("security")
            .args(["find-generic-password", "-s", service, "-w"])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).ok()
    }

    pub fn write(service: &str, body: &str) -> Result<()> {
        let user = std::env::var("USER").unwrap_or_else(|_| "unknown".to_string());
        let out = Command::new("security")
            .args([
                "add-generic-password",
                "-U",
                "-a",
                &user,
                "-s",
                service,
                "-w",
                body,
            ])
            .output()
            .context("cant run `security`")?;
        anyhow::ensure!(
            out.status.success(),
            "keychain write failed - is the login keychain unlocked?"
        );
        Ok(())
    }

    pub fn delete(service: &str) {
        let _ = Command::new("security")
            .args(["delete-generic-password", "-s", service])
            .output();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("accio-store-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    fn creds(expires_at: i64) -> Value {
        json!({"claudeAiOauth": {"accessToken": format!("a{expires_at}"), "refreshToken": "r", "expiresAt": expires_at}})
    }

    #[test]
    fn offer_keeps_the_newest_chain_and_leaves_no_lock_behind() {
        let dir = tdir("offer");
        let slot = Slot::store(&dir);
        assert!(!slot.offer(&json!({"mcpOAuth": {}})).unwrap());
        assert!(!dir.exists());
        assert!(slot.offer(&creds(2)).unwrap());
        assert!(!slot.offer(&creds(1)).unwrap());
        assert!(!slot.offer(&creds(2)).unwrap());
        assert!(slot.offer(&creds(3)).unwrap());
        assert_eq!(expires_at(&slot.read().unwrap()), 3);
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        assert_eq!(slot.take().unwrap().map(|c| expires_at(&c)), Some(3));
        assert!(slot.read().is_none());
        assert!(slot.take().unwrap().is_none());
    }

    #[test]
    fn lock_waits_for_claude_and_reclaims_abandoned_locks() {
        let dir = tdir("lock");
        let slot = Slot::store(&dir);
        slot.write(&creds(1)).unwrap();
        let held = dir.join(".oauth_refresh.lock");
        fs::create_dir(&held).unwrap();
        let err = slot.lock_within(Duration::from_millis(300)).unwrap_err();
        assert!(err.to_string().contains("another claude process"));
        assert!(held.exists());
        let old = SystemTime::now() - Duration::from_secs(120);
        let stamp = filetime(old);
        std::process::Command::new("touch")
            .args(["-d", &stamp, held.to_str().unwrap()])
            .status()
            .unwrap();
        {
            let _lock = slot.lock_within(Duration::from_millis(300)).unwrap();
            assert!(held.exists());
            assert!(dir.join(".credentials.json.lock").exists());
        }
        assert!(!held.exists());
        assert!(!dir.join(".credentials.json.lock").exists());
    }

    fn filetime(t: SystemTime) -> String {
        let secs = t.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs();
        format!("@{secs}")
    }

    #[test]
    fn fresh_token_reads_without_refreshing_when_valid() {
        let dir = tdir("fresh");
        let slot = Slot::store(&dir);
        let soon = Utc::now().timestamp_millis() + 3_600_000;
        slot.write(&creds(soon)).unwrap();
        assert_eq!(slot.fresh_token().unwrap(), format!("a{soon}"));
        let empty = Slot::store(&tdir("fresh-empty"));
        assert!(empty
            .fresh_token()
            .unwrap_err()
            .to_string()
            .contains("no credentials"));
    }
}
