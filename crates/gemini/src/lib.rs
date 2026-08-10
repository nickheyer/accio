//! gemini provider - swaps the gemini cli's oauth files

use std::collections::BTreeMap;

use accio_provider::files;
use accio_provider::{Backend, Job, Provider, Swap};
use anyhow::Result;

const FILES: &[&str] = &["~/.gemini/oauth_creds.json", "~/.gemini/google_accounts.json"];
const LOGIN: &[&str] = &["gemini"];

pub fn provider() -> Result<impl Provider> {
    Swap::load(Gemini)
}

struct Gemini;

impl Backend for Gemini {
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
}
