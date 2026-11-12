//! What every provider looks like to accio

pub mod files;
mod fsutil;
mod overlay;
pub mod session;
mod swap;
mod usage;

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use anyhow::{bail, Result};
use serde_json::Value;

pub use fsutil::{contract, expand, read_json, sanitize_name, write_atomic};
pub use overlay::canon_str;
pub use swap::{Backend, Swap};
pub use usage::{
    common_prefix, humanize_until, humanize_when, label, local_stamp, parse_usage, Metric,
    MetricValue, Scale, Until, Usage,
};

// One setting a hand-configured profile can carry
#[derive(Clone)]
pub struct Knob {
    pub name: String,
    pub hint: String,
    pub secret: bool,
}

impl Knob {
    pub fn new(name: &str, hint: &str) -> Self {
        Knob {
            name: name.to_string(),
            hint: hint.to_string(),
            secret: false,
        }
    }

    pub fn secret(name: &str, hint: &str) -> Self {
        Knob {
            name: name.to_string(),
            hint: hint.to_string(),
            secret: true,
        }
    }
}

pub struct Account {
    pub name: String,
    pub email: Option<String>,
    pub plan: Option<String>,
}

// What a harness process on the live login is doing right now
#[derive(Clone, Debug, PartialEq)]
pub struct SessionState {
    pub id: String,
    pub state: String,
    pub mid_turn: bool,
}

// A harness process on the live login and what a pending login change does to it
#[derive(Clone, Debug)]
pub struct Session {
    pub now: SessionState,
    pub name: String,
    pub dir: String,
    pub effect: String,
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

    // Build a child command using only a private, caller-owned directory.
    fn session(&self, _idx: usize, _dir: &Path) -> Result<Command> {
        bail!("'{}' does not support session launches", self.name())
    }

    // settings a configured profile can set instead of logging in
    fn knobs(&self) -> Vec<Knob> {
        Vec::new()
    }

    // Whether add can run a login instead of a configure form
    fn can_login(&self) -> bool {
        true
    }

    // store a profile built from knob values, nothing goes live yet
    fn configure(
        &mut self,
        _name: Option<&str>,
        _values: &BTreeMap<String, String>,
    ) -> Result<String> {
        bail!("'{}' does not support configured profiles", self.name())
    }

    // configure editable profile, empty when not editable
    fn values(&self, _name: &str) -> BTreeMap<String, String> {
        BTreeMap::new()
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

    // Label value pairs the tab's info panel shows
    fn info(&self) -> Vec<(String, String)> {
        Vec::new()
    }

    // Sessions on the live login and what moving it to the target, or to a fresh login, does to each
    fn sessions(&self, _target: Option<usize>) -> Vec<Session> {
        Vec::new()
    }

    // The same sessions with only their state read, cheap enough to poll
    fn session_states(&self) -> Vec<SessionState> {
        Vec::new()
    }
}
