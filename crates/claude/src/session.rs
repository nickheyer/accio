//! A launch never writes to the live settings, credentials or keychain.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;

use accio_provider::session::{copy_resources, read_object, require_version};
use accio_provider::write_atomic;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use super::{store_dir, Claude};
use crate::store::Slot;

// Empty values in the command's settings also mask credentials from project settings.
const AUTH_ENV: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_CUSTOM_HEADERS",
    "ANTHROPIC_MODEL",
    "ANTHROPIC_SMALL_FAST_MODEL",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_REFRESH_TOKEN",
    "CLAUDE_CODE_OAUTH_SCOPES",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
    "CLAUDE_CODE_USE_MANTLE",
    "CLAUDE_CODE_USE_ANTHROPIC_AWS",
    "CLAUDE_CODE_API_KEY_HELPER_FD",
    "CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR",
];

fn profile_env(key: &str) -> bool {
    key.starts_with("ANTHROPIC_")
        || key.starts_with("CLAUDE_CODE_OAUTH_")
        || key.starts_with("CLAUDE_CODE_USE_")
        || AUTH_ENV.contains(&key)
}

fn reserved(key: &str) -> bool {
    matches!(
        key,
        "CLAUDE_CONFIG_DIR" | "CLAUDE_SECURESTORAGE_CONFIG_DIR" | "HOME" | "USERPROFILE"
    )
}

// Env keys any saved profile sets through a settings.json merge
fn claimed_env(profiles: &[&BTreeMap<String, String>]) -> BTreeSet<String> {
    profiles
        .iter()
        .flat_map(|files| files.iter())
        .filter(|(k, _)| k.starts_with("merge:") && k.ends_with("/settings.json"))
        .filter_map(|(_, body)| serde_json::from_str::<Value>(body).ok())
        .filter_map(|doc| {
            doc.get("env")?
                .as_object()
                .map(|env| env.keys().cloned().collect::<Vec<_>>())
        })
        .flatten()
        .collect()
}

// Scrubbed keys leave the copy and get masked so no other profile's settings reach the session
fn scrub_settings(
    settings: &mut Value,
    scrub: &dyn Fn(&str) -> bool,
    overrides: &mut BTreeMap<String, String>,
) {
    if let Some(env) = settings.get_mut("env").and_then(Value::as_object_mut) {
        env.retain(|key, _| {
            if scrub(key) || reserved(key) {
                if !reserved(key) {
                    overrides.insert(key.clone(), String::new());
                }
                false
            } else {
                true
            }
        });
    }
}

pub(super) fn prepare(
    claude: &Claude,
    files: &BTreeMap<String, String>,
    profiles: &[&BTreeMap<String, String>],
    dir: &Path,
) -> Result<Command> {
    require_version(
        Command::new("claude")
            .env("CLAUDE_CONFIG_DIR", dir)
            .env("CLAUDE_SECURESTORAGE_CONFIG_DIR", dir),
        (2, 1, 144),
        "separate credential storage",
    )?;
    let source = claude
        .settings_path
        .parent()
        .context("missing Claude config directory")?;
    let mut settings = read_object(&claude.settings_path)?;
    let mut overrides = BTreeMap::<String, String>::new();
    for key in AUTH_ENV {
        overrides.insert((*key).into(), String::new());
    }
    // Every profile's environment leaves this copy and the inherited env, never the original
    let claimed = claimed_env(profiles);
    let scrub = |key: &str| profile_env(key) || claimed.contains(key);
    scrub_settings(&mut settings, &scrub, &mut overrides);
    settings.as_object_mut().unwrap().remove("apiKeyHelper");

    let mut command = Command::new("claude");
    for (key, _) in std::env::vars_os() {
        if let Some(name) = key.to_str().filter(|k| scrub(k)) {
            overrides.insert(name.into(), String::new());
            command.env_remove(&key);
        }
    }

    // Every session of an account shares its directory, so claude code refreshes one token chain
    let store = store_dir(files);
    if let Some(store) = &store {
        let creds = Slot::store(store)
            .read()
            .context("saved Claude account has no credentials - add the account again")?;
        anyhow::ensure!(
            creds
                .pointer("/claudeAiOauth/accessToken")
                .and_then(Value::as_str)
                .is_some_and(|s| !s.is_empty()),
            "saved Claude account has no OAuth access token - add the account again"
        );
    } else {
        let mut found = false;
        for (key, body) in files {
            // Profile paths are labels here: never write through a stored absolute path.
            if !key.starts_with("merge:") || !key.ends_with("/settings.json") {
                bail!("unsupported Claude session profile entry '{key}'");
            }
            let patch: Value =
                serde_json::from_str(body).context("invalid saved Claude settings")?;
            let env = patch
                .get("env")
                .and_then(Value::as_object)
                .context("Claude profile has no env settings")?;
            for (key, value) in env {
                anyhow::ensure!(
                    !reserved(key),
                    "'{key}' cannot be overridden in an isolated session"
                );
                overrides.insert(
                    key.clone(),
                    value
                        .as_str()
                        .context("profile env values must be strings")?
                        .into(),
                );
                found = true;
            }
        }
        anyhow::ensure!(
            found,
            "Claude profile has no credentials or environment settings"
        );
    }

    let mut config = read_object(&claude.claude_json_path)?;
    let object = config.as_object_mut().unwrap();
    object.remove("oauthAccount");
    object.remove("primaryApiKey");
    if let Some(account) = files.get("oauth_account").filter(|_| store.is_some()) {
        object.insert("oauthAccount".into(), serde_json::from_str(account)?);
    }
    write_atomic(
        &dir.join(".claude.json"),
        serde_json::to_vec(&config)?.as_slice(),
    )?;
    write_atomic(
        &dir.join("settings.json"),
        serde_json::to_vec(&settings)?.as_slice(),
    )?;

    // Snapshot user instructions and extensions; no writable links to the live config.
    copy_resources(
        source,
        dir,
        &[
            "CLAUDE.md",
            "agents",
            "commands",
            "skills",
            "rules",
            "output-styles",
            "plugins",
        ],
    )?;

    let path = dir
        .to_str()
        .context("Claude session directory must be UTF-8")?;
    let secure = match &store {
        Some(store) => store
            .to_str()
            .context("Claude account directory must be UTF-8")?,
        None => path,
    };
    overrides.insert("CLAUDE_CONFIG_DIR".into(), path.into());
    overrides.insert("CLAUDE_SECURESTORAGE_CONFIG_DIR".into(), secure.into());
    command.envs(&overrides);
    // Command-line settings win over user/project settings without putting secrets in argv.
    let launch_settings = dir.join("accio-settings.json");
    write_atomic(
        &launch_settings,
        serde_json::to_vec(&json!({
            "env": overrides, "apiKeyHelper": ""
        }))?
        .as_slice(),
    )?;
    command.arg("--settings").arg(launch_settings);
    Ok(command)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claimed_env_collects_every_profiles_settings_keys() {
        let local = BTreeMap::from([(
            "merge:~/.claude/settings.json".to_string(),
            json!({"env": {"ANTHROPIC_BASE_URL": "x", "CLAUDE_CODE_AUTO_COMPACT_WINDOW": "1"}})
                .to_string(),
        )]);
        let moved = BTreeMap::from([
            (
                "merge:/other/settings.json".to_string(),
                json!({"env": {"EXTRA": "y"}}).to_string(),
            ),
            ("/elsewhere/file".to_string(), "not settings".to_string()),
        ]);
        let login = BTreeMap::from([("credentials_dir".to_string(), "{}".to_string())]);
        assert_eq!(
            claimed_env(&[&local, &moved, &login]),
            BTreeSet::from([
                "ANTHROPIC_BASE_URL".to_string(),
                "CLAUDE_CODE_AUTO_COMPACT_WINDOW".to_string(),
                "EXTRA".to_string(),
            ])
        );
        assert!(claimed_env(&[]).is_empty());
    }

    #[test]
    fn scrub_drops_and_masks_claimed_keys_keeping_the_rest() {
        let mut settings = json!({"theme": "dark", "env": {
            "ANTHROPIC_MODEL": "m",
            "CLAUDE_CODE_AUTO_COMPACT_WINDOW": "1",
            "CLAUDE_CONFIG_DIR": "/x",
            "EDITOR": "vi"
        }});
        let claimed = BTreeSet::from(["CLAUDE_CODE_AUTO_COMPACT_WINDOW".to_string()]);
        let scrub = |key: &str| profile_env(key) || claimed.contains(key);
        let mut overrides = BTreeMap::new();
        scrub_settings(&mut settings, &scrub, &mut overrides);
        assert_eq!(settings["env"], json!({"EDITOR": "vi"}));
        assert_eq!(settings["theme"], "dark");
        assert_eq!(
            overrides,
            BTreeMap::from([
                ("ANTHROPIC_MODEL".to_string(), String::new()),
                ("CLAUDE_CODE_AUTO_COMPACT_WINDOW".to_string(), String::new()),
            ])
        );
        let mut bare = json!({"theme": "dark"});
        scrub_settings(&mut bare, &scrub, &mut overrides);
        assert_eq!(bare, json!({"theme": "dark"}));
    }
}
