//! grok provider - swaps the grok cli's auth file

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use accio_provider::files;
use accio_provider::{Backend, Job, Knob, Provider, Swap};
use anyhow::{Context, Result};

const FILES: &[&str] = &["~/.grok/auth.json"];
const LOGIN: &[&str] = &["grok", "login"];

pub fn provider() -> Result<impl Provider> {
    Swap::load(Grok)
}

pub fn session_provider() -> Result<impl Provider> {
    Swap::load_saved(Grok)
}

struct Grok;

impl Backend for Grok {
    fn session(&self, files: &BTreeMap<String, String>, dir: &Path) -> Result<Command> {
        use accio_provider::{session, write_atomic};
        let body = files
            .get(FILES[0])
            .context("Grok profile has no auth.json")?;
        let auth: serde_json::Value =
            serde_json::from_str(body).context("invalid saved Grok credentials")?;
        anyhow::ensure!(auth.is_object(), "Grok auth.json must be an object");
        let source = session::config_dir("GROK_HOME", "~/.grok");
        session::copy_resources(
            &source,
            dir,
            &[
                "config.toml",
                "managed_config.toml",
                "requirements.toml",
                "GROK.md",
                "AGENTS.md",
                "skills",
                "commands",
                "agents",
                "hooks",
                "plugins",
                "mcp.json",
            ],
        )?;
        let mut command = Command::new("grok");
        command
            .env("GROK_HOME", dir)
            .env_remove("XAI_API_KEY")
            .env_remove("GROK_API_KEY")
            .env_remove("GROK_BASE_URL")
            .env_remove("GROK_CLI_CHAT_PROXY_BASE_URL");
        if let Some(key) = auth
            .get("GROK_API_KEY")
            .or_else(|| auth.get("XAI_API_KEY"))
            .and_then(serde_json::Value::as_str)
        {
            // Accio's older configured profiles use GROK_API_KEY; Grok Build reads XAI_API_KEY.
            command.env("XAI_API_KEY", key).env("GROK_API_KEY", key);
            if let Some(url) = auth
                .get("GROK_BASE_URL")
                .and_then(serde_json::Value::as_str)
            {
                command
                    .env("GROK_BASE_URL", url)
                    .env("GROK_CLI_CHAT_PROXY_BASE_URL", url);
            }
        } else {
            write_atomic(&dir.join("auth.json"), body.as_bytes())?;
        }
        Ok(command)
    }

    fn name(&self) -> &str {
        "grok"
    }

    fn read_live(&self) -> BTreeMap<String, String> {
        files::read_live(FILES)
    }

    fn write_live(&self, contents: &BTreeMap<String, String>) -> Result<()> {
        files::write_live(FILES, contents)
    }

    fn login(&self) -> &[&str] {
        LOGIN
    }

    fn fetch(&self, snapshot: BTreeMap<String, String>) -> Job {
        files::facts_job(snapshot)
    }

    fn info(&self) -> Vec<(String, String)> {
        files::info(FILES, LOGIN)
    }

    fn knobs(&self) -> Vec<Knob> {
        vec![
            Knob::secret("GROK_API_KEY", "api key stored in auth.json"),
            Knob::new("GROK_BASE_URL", "endpoint override stored in auth.json"),
        ]
    }

    fn compose(&self, values: &BTreeMap<String, String>) -> BTreeMap<String, String> {
        files::compose_json(FILES[0], values)
    }
}
