//! What every provider looks like to accio

pub mod files;
mod fsutil;
mod overlay;
mod swap;
mod usage;

use std::collections::BTreeMap;

use anyhow::{bail, Result};
use serde_json::Value;

pub use fsutil::{read_json, sanitize_name, write_atomic};
pub use swap::{Backend, Swap};
pub use usage::{humanize_until, parse_usage, Fact, Severity, Usage, Window};

// One setting a hand-configured profile can carry
#[derive(Clone)]
pub struct Knob {
    pub name: String,
    pub hint: String,
    pub secret: bool,
}

impl Knob {
    pub fn new(name: &str, hint: &str) -> Self {
        Knob { name: name.to_string(), hint: hint.to_string(), secret: false }
    }

    pub fn secret(name: &str, hint: &str) -> Self {
        Knob { name: name.to_string(), hint: hint.to_string(), secret: true }
    }
}

pub struct Account {
    pub name: String,
    pub email: Option<String>,
    pub plan: Option<String>,
}

pub struct Outcome {
    pub usage: std::result::Result<Usage, String>,
    // opaque provider state a fetch brought back, handed to absorb_fetch
    pub state: Option<Value>,
}

pub type Job = Box<dyn FnOnce() -> Outcome + Send>;

pub struct Fetch {
    pub account: String,
    pub job: Job,
}

pub trait Provider {
    fn name(&self) -> &str;
    fn accounts(&self) -> Vec<Account>;
    fn active(&self) -> Option<usize>;
    fn activate(&mut self, idx: usize) -> Result<()>;
    fn delete(&mut self, name: &str) -> Result<()>;
    fn add(&mut self, name: Option<&str>) -> Result<String>;

    // settings a configured profile can set instead of logging in
    fn knobs(&self) -> Vec<Knob> {
        Vec::new()
    }

    // store a profile built from knob values, nothing goes live yet
    fn configure(&mut self, _name: Option<&str>, _values: &BTreeMap<String, String>) -> Result<String> {
        bail!("'{}' does not support configured profiles", self.name())
    }

    // pick up whatever changed on disk since we last looked
    fn refresh(&mut self) -> Result<()> {
        Ok(())
    }

    // one background job per account
    fn fetches(&self) -> Vec<Fetch>;

    // persist state a fetch brought back (refreshed tokens and the like)
    fn absorb_fetch(&mut self, _account: &str, _state: Value) -> Result<()> {
        Ok(())
    }

    // label/value pairs shown while the provider has no accounts yet
    fn info(&self) -> Vec<(String, String)> {
        Vec::new()
    }
}
