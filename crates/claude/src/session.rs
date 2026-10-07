//! A launch never writes to the live settings, credentials or keychain.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use accio_provider::session::{copy_resources, read_object, require_version};
use accio_provider::write_atomic;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use super::Claude;

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

pub(super) fn prepare(
    claude: &Claude,
    files: &BTreeMap<String, String>,
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
        .creds_path
        .parent()
        .context("missing Claude config directory")?;
    let mut settings = read_object(&source.join("settings.json"))?;
    let mut overrides = BTreeMap::<String, String>::new();
    for key in AUTH_ENV {
        overrides.insert((*key).into(), String::new());
    }
    // Remove the outgoing profile's environment from this copy, never from the original.
    if let Some(env) = settings.get_mut("env").and_then(Value::as_object_mut) {
        env.retain(|key, _| {
            if profile_env(key) || reserved(key) {
                if !reserved(key) {
                    overrides.insert(key.clone(), String::new());
                }
                false
            } else {
                true
            }
        });
    }
    settings.as_object_mut().unwrap().remove("apiKeyHelper");

    let mut command = Command::new("claude");
    for (key, _) in std::env::vars_os() {
        if key.to_str().is_some_and(profile_env) {
            if let Some(key) = key.to_str() {
                overrides.insert(key.into(), String::new());
            }
            command.env_remove(key);
        }
    }

    let credentials = files.get("credentials");
    if let Some(body) = credentials {
        let creds: Value =
            serde_json::from_str(body).context("invalid saved Claude credentials")?;
        anyhow::ensure!(
            creds
                .pointer("/claudeAiOauth/accessToken")
                .and_then(Value::as_str)
                .is_some_and(|s| !s.is_empty()),
            "saved Claude account has no OAuth access token - re-add the account"
        );
        write_atomic(&dir.join(".credentials.json"), body.as_bytes())?;
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
    if let Some(account) = files.get("oauth_account").filter(|_| credentials.is_some()) {
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
    overrides.insert("CLAUDE_CONFIG_DIR".into(), path.into());
    overrides.insert("CLAUDE_SECURESTORAGE_CONFIG_DIR".into(), path.into());
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
