//! grok provider - swaps the grok cli's auth file

use std::collections::BTreeMap;

use accio_provider::files;
use accio_provider::{Backend, Job, Provider, Swap};
use anyhow::Result;

const FILES: &[&str] = &["~/.grok/auth.json"];
const LOGIN: &[&str] = &["grok", "login"];

pub fn provider() -> Result<impl Provider> {
    Swap::load(Grok)
}

struct Grok;

impl Backend for Grok {
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
}
