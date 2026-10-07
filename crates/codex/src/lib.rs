//! codex provider - swaps the codex cli's auth file

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use accio_provider::files;
use accio_provider::{Backend, Job, Knob, Provider, Swap};
use anyhow::{Context, Result};

const FILES: &[&str] = &["~/.codex/auth.json"];
const LOGIN: &[&str] = &["codex", "login"];

pub fn provider() -> Result<impl Provider> {
    Swap::load(Codex)
}

pub fn session_provider() -> Result<impl Provider> {
    Swap::load_saved(Codex)
}

struct Codex;

impl Backend for Codex {
    fn session(&self, files: &BTreeMap<String, String>, dir: &Path) -> Result<Command> {
        use accio_provider::{session, write_atomic};
        let body = files
            .get(FILES[0])
            .context("Codex profile has no auth.json")?;
        let auth: serde_json::Value =
            serde_json::from_str(body).context("invalid saved Codex credentials")?;
        anyhow::ensure!(auth.is_object(), "Codex auth.json must be an object");
        let source = session::config_dir("CODEX_HOME", "~/.codex");
        session::copy_resources(
            &source,
            dir,
            &[
                "config.toml",
                "AGENTS.md",
                "AGENTS.override.md",
                "skills",
                "rules",
                "prompts",
                "agents",
            ],
        )?;
        write_atomic(&dir.join("auth.json"), body.as_bytes())?;
        let mut command = Command::new("codex");
        command
            .env("CODEX_HOME", dir)
            .env_remove("OPENAI_API_KEY")
            .env_remove("CODEX_API_KEY")
            .env_remove("OPENAI_BASE_URL")
            .env_remove("CODEX_AUTH_TOKEN")
            .env_remove("CODEX_AUTH_TOKEN_FILE")
            .args(["-c", "cli_auth_credentials_store=\"file\""]);
        Ok(command)
    }

    fn name(&self) -> &str {
        "codex"
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
        vec![Knob::secret(
            "OPENAI_API_KEY",
            "api key stored in auth.json",
        )]
    }

    fn compose(&self, values: &BTreeMap<String, String>) -> BTreeMap<String, String> {
        files::compose_json(FILES[0], values)
    }
}
