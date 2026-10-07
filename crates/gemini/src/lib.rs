//! gemini provider - swaps the gemini cli's oauth files

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use accio_provider::files;
use accio_provider::{Backend, Job, Knob, Provider, Swap};
use anyhow::{Context, Result};
use serde_json::{json, Value};

const FILES: &[&str] = &[
    "~/.gemini/oauth_creds.json",
    "~/.gemini/google_accounts.json",
];
const LOGIN: &[&str] = &["gemini"];
const ENV_FILE: &str = "~/.gemini/.env";

pub fn provider() -> Result<impl Provider> {
    Swap::load(Gemini)
}

pub fn session_provider() -> Result<impl Provider> {
    Swap::load_saved(Gemini)
}

struct Gemini;

impl Backend for Gemini {
    fn session(&self, files: &BTreeMap<String, String>, dir: &Path) -> Result<Command> {
        use accio_provider::{session, write_atomic};
        session::require_version(
            Command::new("gemini").env("GEMINI_CLI_HOME", dir),
            (0, 30, 0),
            "GEMINI_CLI_HOME support",
        )?;
        let source = session::config_dir("GEMINI_CLI_HOME", "~/").join(".gemini");
        let target = dir.join(".gemini");
        std::fs::create_dir(&target)?;
        session::copy_resources(
            &source,
            &target,
            &[
                "GEMINI.md",
                "commands",
                "skills",
                "agents",
                "policies",
                "extensions",
                "trustedFolders.json",
            ],
        )?;
        let mut settings = session::read_object(&source.join("settings.json"))?;
        let mut command = Command::new("gemini");
        // Defined empty values also prevent a project's .env from filling in another account.
        for key in [
            "GEMINI_API_KEY",
            "GOOGLE_API_KEY",
            "GOOGLE_GEMINI_BASE_URL",
            "GOOGLE_VERTEX_BASE_URL",
            "GOOGLE_GENAI_USE_VERTEXAI",
            "GOOGLE_GENAI_USE_GCA",
            "GOOGLE_APPLICATION_CREDENTIALS",
        ] {
            command.env(key, "");
        }
        let auth_type = if let Some(env) = files.get(ENV_FILE) {
            for line in env
                .lines()
                .filter(|l| !l.trim().is_empty() && !l.trim().starts_with('#'))
            {
                let (key, value) = line
                    .split_once('=')
                    .context("invalid saved Gemini environment")?;
                let key = key.trim();
                anyhow::ensure!(
                    !matches!(key, "HOME" | "USERPROFILE")
                        && !key.starts_with("GEMINI_CLI_")
                        && key != "GEMINI_FORCE_FILE_STORAGE",
                    "'{key}' cannot be overridden in an isolated session"
                );
                command.env(key, value.trim());
            }
            "gemini-api-key"
        } else {
            anyhow::ensure!(
                files.contains_key(FILES[0]),
                "Gemini profile has no OAuth credentials"
            );
            for path in FILES {
                if let Some(body) = files.get(*path) {
                    let _: Value =
                        serde_json::from_str(body).context("invalid saved Gemini credentials")?;
                    let filename = Path::new(path)
                        .file_name()
                        .context("missing credential filename")?;
                    write_atomic(&target.join(filename), body.as_bytes())?;
                }
            }
            "oauth-personal"
        };
        let security = settings
            .as_object_mut()
            .unwrap()
            .entry("security")
            .or_insert_with(|| json!({}));
        let auth = security
            .as_object_mut()
            .context("Gemini security settings must be an object")?
            .entry("auth")
            .or_insert_with(|| json!({}));
        let auth = auth
            .as_object_mut()
            .context("Gemini auth settings must be an object")?;
        auth.insert("selectedType".into(), auth_type.into());
        auth.insert("useExternal".into(), false.into());
        write_atomic(
            &target.join("settings.json"),
            &serde_json::to_vec(&settings)?,
        )?;
        command
            .env("GEMINI_CLI_HOME", dir)
            .env("GEMINI_FORCE_FILE_STORAGE", "true")
            .env("GEMINI_FORCE_ENCRYPTED_FILE_STORAGE", "false")
            .env("GEMINI_CLI_AUTH_OVERRIDE", auth_type)
            .env(
                "GEMINI_CLI_TRUSTED_FOLDERS_PATH",
                target.join("trustedFolders.json"),
            );
        Ok(command)
    }

    fn name(&self) -> &str {
        "gemini"
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
            Knob::secret("GEMINI_API_KEY", "api key the cli reads from its env file"),
            Knob::new("GOOGLE_GEMINI_BASE_URL", "endpoint override"),
        ]
    }

    fn compose(&self, values: &BTreeMap<String, String>) -> BTreeMap<String, String> {
        files::compose_dotenv(ENV_FILE, values)
    }
}
