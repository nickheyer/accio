mod tui;

use std::collections::BTreeMap;

use accio_provider::Provider;
use anyhow::{bail, Context, Result};

// every provider accio knows about, one crate each
fn providers() -> Result<Vec<Box<dyn Provider>>> {
    Ok(vec![
        Box::new(accio_claude::provider()?),
        Box::new(accio_codex::provider()?),
        Box::new(accio_grok::provider()?),
        Box::new(accio_gemini::provider()?),
    ])
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        None => tui::run(),
        Some("list") => cmd_list(),
        Some("add") => cmd_add(args.get(1), args.get(2)),
        Some("configure") => cmd_configure(&args[1..]),
        Some("remove") => cmd_remove(args.get(1)),
        Some("help" | "-h" | "--help") => {
            print_help();
            Ok(())
        }
        Some(name) => cmd_switch(name),
    };
    if let Err(e) = result {
        eprintln!("accio: {e:#}");
        std::process::exit(1);
    }
}

fn cmd_list() -> Result<()> {
    let providers = providers()?;
    if providers.iter().all(|p| p.accounts().is_empty()) {
        println!("no accounts - run `accio add` or just `accio`");
        return Ok(());
    }
    for p in &providers {
        for (i, a) in p.accounts().iter().enumerate() {
            let marker = if p.active() == Some(i) { "*" } else { " " };
            println!(
                "{marker} {:<8} {:<14} {:<30} {}",
                p.name(),
                a.name,
                a.email.as_deref().unwrap_or("-"),
                a.plan.as_deref().unwrap_or("")
            );
        }
    }
    Ok(())
}

// "codex/work" pins the provider, bare "work" searches everywhere
fn resolve(providers: &[Box<dyn Provider>], spec: &str) -> Result<(usize, usize)> {
    let (want_provider, want_account) = match spec.split_once('/') {
        Some((p, a)) => (Some(p), a),
        None => (None, spec),
    };
    let matches: Vec<(usize, usize)> = providers
        .iter()
        .enumerate()
        .filter(|(_, p)| want_provider.map_or(true, |w| p.name() == w))
        .flat_map(|(pi, p)| {
            p.accounts()
                .iter()
                .enumerate()
                .filter(|(_, a)| a.name == want_account)
                .map(|(ai, _)| (pi, ai))
                .collect::<Vec<_>>()
        })
        .collect();
    match matches.as_slice() {
        [] => bail!("no account named '{spec}' - see `accio list`"),
        [one] => Ok(*one),
        many => {
            let names: Vec<String> = many
                .iter()
                .map(|&(pi, ai)| {
                    format!("{}/{}", providers[pi].name(), providers[pi].accounts()[ai].name)
                })
                .collect();
            bail!("'{spec}' is ambiguous - try one of: {}", names.join(", "))
        }
    }
}

fn cmd_switch(spec: &str) -> Result<()> {
    let mut providers = providers()?;
    let (pi, ai) = resolve(&providers, spec)?;
    if providers[pi].active() == Some(ai) {
        println!("'{spec}' is already active");
        return Ok(());
    }
    providers[pi].activate(ai)?;
    println!("switched to '{spec}'");
    Ok(())
}

fn cmd_add(first: Option<&String>, second: Option<&String>) -> Result<()> {
    let mut providers = providers()?;
    // `accio add codex [name]` picks a provider, anything else is a name for the first one
    let (pi, name) = match first {
        Some(f) => match providers.iter().position(|p| p.name() == f.as_str()) {
            Some(pi) => (pi, second.map(String::as_str)),
            None => (0, Some(f.as_str())),
        },
        None => (0, None),
    };
    let msg = providers[pi].add(name)?;
    println!("{msg}");
    Ok(())
}

// store a profile from knob values instead of a login, eg claude behind a proxy
fn cmd_configure(args: &[String]) -> Result<()> {
    let provider = args
        .first()
        .context("usage: accio configure <provider> [name] KEY=VALUE...")?;
    let mut providers = providers()?;
    let pi = providers
        .iter()
        .position(|p| p.name() == provider.as_str())
        .with_context(|| format!("no provider named '{provider}'"))?;
    let mut name = None;
    let mut values = BTreeMap::new();
    for arg in &args[1..] {
        match arg.split_once('=') {
            Some((k, v)) => {
                values.insert(k.to_string(), v.to_string());
            }
            None => name = Some(arg.as_str()),
        }
    }
    if values.is_empty() {
        let knobs = providers[pi].knobs();
        if knobs.is_empty() {
            bail!("'{provider}' has no configurable settings");
        }
        println!("knobs for {provider}. Any KEY=VALUE works, ex:\n");
        for k in &knobs {
            println!("  {:<28} {}", k.name, k.hint);
        }
        println!("\nusage: accio configure {provider} [name] KEY=VALUE...");
        return Ok(());
    }
    let msg = providers[pi].configure(name, &values)?;
    println!("{msg}");
    Ok(())
}

fn cmd_remove(name: Option<&String>) -> Result<()> {
    let spec = name.context("usage: accio remove <name>")?;
    let mut providers = providers()?;
    let (pi, ai) = resolve(&providers, spec)?;
    let account = providers[pi].accounts()[ai].name.clone();
    providers[pi].delete(&account)?;
    println!("removed '{spec}'");
    Ok(())
}

fn print_help() {
    println!(
        "accio - switch between provider accounts without re-auth

usage:
  accio                                           open the TUI
  accio <your other account>                      make account the live account (provider/name if ambiguous)
  accio list                                      list accounts
  accio add [provider] [name]                     add currently logged in account or log one in if specified
  accio configure <provider> [name] KEY=VALUE...  for advance profile configuration
  accio remove <your other account>               remove an account from accio
  accio help                                      show this help"
    );
}
