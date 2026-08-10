//! What every provider looks like to accio

pub mod files;
mod fsutil;
mod swap;
mod usage;

use anyhow::Result;
use serde_json::Value;

pub use fsutil::{read_json, sanitize_name, write_atomic};
pub use swap::{Backend, Swap};
pub use usage::{humanize_until, parse_usage, Fact, Severity, Usage, Window};

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
