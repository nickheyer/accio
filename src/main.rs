mod hold;
mod launch;
mod tui;

use std::collections::BTreeMap;
use std::io::{IsTerminal, Write};
use std::time::Duration;

use accio_custom::Registry;
use accio_provider::{sanitize_name, Provider};
use anyhow::{bail, Context, Result};

use hold::{Hold, Wait};

fn providers() -> Result<Vec<Box<dyn Provider>>> {
    providers_in(&Registry::load()?)
}

// every enabled provider, built ins from their crates and the rest from definitions
fn providers_in(registry: &Registry) -> Result<Vec<Box<dyn Provider>>> {
    let mut providers: Vec<Box<dyn Provider>> = Vec::new();
    for def in registry.enabled() {
        providers.push(match def.name.as_str() {
            "claude" => Box::new(accio_claude::provider()?),
            "codex" => Box::new(accio_codex::provider()?),
            "grok" => Box::new(accio_grok::provider()?),
            "gemini" => Box::new(accio_gemini::provider()?),
            _ => Box::new(accio_custom::provider(def)?),
        });
    }
    anyhow::ensure!(
        !providers.is_empty(),
        "every provider is disabled - see `accio provider list`"
    );
    Ok(providers)
}

fn main() {
    if let Err(e) = run() {
        eprintln!("accio: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    // A broken definition must surface here, help still works without one
    if let Some(name) = args
        .first()
        .and_then(|a| a.to_str())
        .filter(|n| !matches!(*n, "help" | "-h" | "--help"))
    {
        if let Some(def) = Registry::load()?.get(name).cloned() {
            return launch::run(&def, &args[1..]);
        }
    }
    let mut args: Vec<String> = args
        .into_iter()
        .map(|a| {
            a.into_string()
                .map_err(|_| anyhow::anyhow!("account commands require UTF-8 arguments"))
        })
        .collect::<Result<_>>()?;
    let wait = take_wait(&mut args)?;
    let command = args.first().map(String::as_str);
    if wait.is_some()
        && matches!(
            command,
            None | Some("list" | "remove" | "provider" | "help" | "-h" | "--help")
        )
    {
        bail!("--now and --wait only go with a switch, add or configure");
    }
    match command {
        None => tui::run(),
        Some("list") => cmd_list(),
        Some("add") => cmd_add(args.get(1), args.get(2), wait),
        Some("configure") => cmd_configure(&args[1..], wait),
        Some("remove") => cmd_remove(args.get(1)),
        Some("provider") => cmd_provider(&args[1..]),
        Some("help" | "-h" | "--help") => {
            print_help();
            Ok(())
        }
        Some(name) => cmd_switch(name, wait),
    }
}

// Lifts --now and --wait turns|close out of the arguments
fn take_wait(args: &mut Vec<String>) -> Result<Option<Wait>> {
    let mut wait = None;
    let mut i = 0;
    while i < args.len() {
        match (args[i].as_str(), args.get(i + 1).map(String::as_str)) {
            ("--now", _) => {
                wait = Some(Wait::Now);
                args.remove(i);
            }
            ("--wait", Some("turns")) => {
                wait = Some(Wait::Turn);
                args.drain(i..i + 2);
            }
            ("--wait", Some("close")) => {
                wait = Some(Wait::Close);
                args.drain(i..i + 2);
            }
            ("--wait", _) => bail!("usage: --wait turns|close"),
            _ => i += 1,
        }
    }
    Ok(wait)
}

enum Gate {
    Clear,
    Waited,
    Cancelled,
}

// Lists the sessions a live login change lands on and sits out the ones chosen to wait for
fn gate(
    provider: &dyn Provider,
    target: Option<usize>,
    what: &str,
    wait: Option<Wait>,
) -> Result<Gate> {
    let sessions = provider.sessions(target);
    if sessions.is_empty() {
        return Ok(Gate::Clear);
    }
    let count = match sessions.len() {
        1 => "1 session".to_string(),
        n => format!("{n} sessions"),
    };
    eprintln!("{count} on the live {} login, {what}:", provider.name());
    for (i, s) in sessions.iter().enumerate() {
        eprintln!("  {}  {}  {}  {}", i + 1, s.name, s.now.state, s.dir);
        eprintln!("       {}", s.effect);
    }
    // Unasked, a change waits out the turns already in progress, the same as the TUI starts with
    let turns: Vec<Wait> = sessions.iter().map(|s| Wait::initial(&s.now)).collect();
    let asked = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    let waits = match wait {
        Some(wait) => vec![wait; sessions.len()],
        None if asked => match ask(turns)? {
            Some(waits) => waits,
            None => return Ok(Gate::Cancelled),
        },
        None => turns,
    };
    let holds: Vec<Hold> = sessions
        .iter()
        .zip(waits)
        .map(|(s, wait)| Hold {
            id: s.now.id.clone(),
            name: s.name.clone(),
            wait,
        })
        .collect();
    let mut told = String::new();
    loop {
        let until = hold::until(&holds, &provider.session_states());
        if until.is_empty() {
            break;
        }
        if until != told {
            eprintln!("holding until {until} (ctrl-c calls it off)");
            told = until;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    Ok(if told.is_empty() {
        Gate::Clear
    } else {
        Gate::Waited
    })
}

// Reads which sessions to wait on from the terminal, none when the change is called off
fn ask(turns: Vec<Wait>) -> Result<Option<Vec<Wait>>> {
    let count = turns.len();
    loop {
        eprint!("enter after turns in progress · n now · c after all close · 1,2 after those close · q cancel > ");
        std::io::stderr().flush()?;
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line)? == 0 {
            return Ok(None);
        }
        match line.trim() {
            "" => return Ok(Some(turns)),
            "n" => return Ok(Some(vec![Wait::Now; count])),
            "c" => return Ok(Some(vec![Wait::Close; count])),
            "q" => return Ok(None),
            picks => {
                if let Some(waits) = picked(picks, count) {
                    return Ok(Some(waits));
                }
                eprintln!("pick sessions by number, 1 to {count}");
            }
        }
    }
}

// Close waits for the numbered sessions, none when any number is off the list
fn picked(picks: &str, count: usize) -> Option<Vec<Wait>> {
    let mut waits = vec![Wait::Now; count];
    for pick in picks.split([',', ' ']).filter(|p| !p.is_empty()) {
        let n: usize = pick.parse().ok()?;
        *waits.get_mut(n.checked_sub(1)?)? = Wait::Close;
    }
    Some(waits)
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
        .filter(|(_, p)| want_provider.is_none_or(|w| p.name() == w))
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
                    format!(
                        "{}/{}",
                        providers[pi].name(),
                        providers[pi].accounts()[ai].name
                    )
                })
                .collect();
            bail!("'{spec}' is ambiguous - try one of: {}", names.join(", "))
        }
    }
}

fn cmd_switch(spec: &str, wait: Option<Wait>) -> Result<()> {
    let mut providers = providers()?;
    let (mut pi, mut ai) = resolve(&providers, spec)?;
    if providers[pi].active() == Some(ai) {
        println!("'{spec}' is already active");
        return Ok(());
    }
    match gate(
        &*providers[pi],
        Some(ai),
        &format!("switching to '{spec}'"),
        wait,
    )? {
        Gate::Clear => {}
        Gate::Cancelled => {
            println!("switch to '{spec}' cancelled");
            return Ok(());
        }
        // Accounts may have changed on disk during the wait
        Gate::Waited => {
            providers = self::providers()?;
            (pi, ai) = resolve(&providers, spec)?;
        }
    }
    providers[pi].activate(ai)?;
    println!("switched to '{spec}'");
    Ok(())
}

fn cmd_add(first: Option<&String>, second: Option<&String>, wait: Option<Wait>) -> Result<()> {
    let mut providers = providers()?;
    // `accio add codex [name]` picks a provider, anything else is a name for the first one
    let (pi, name) = match first {
        Some(f) => match providers.iter().position(|p| p.name() == f.as_str()) {
            Some(pi) => (pi, second.map(String::as_str)),
            None => (0, Some(f.as_str())),
        },
        None => (0, None),
    };
    let provider = providers[pi].name().to_string();
    match gate(&*providers[pi], None, "logging in a new account", wait)? {
        Gate::Clear => {}
        Gate::Cancelled => {
            println!("login cancelled");
            return Ok(());
        }
        Gate::Waited => providers = reloaded(&provider)?,
    }
    let pi = position(&providers, &provider)?;
    let msg = providers[pi].add(name)?;
    println!("{msg}");
    Ok(())
}

fn position(providers: &[Box<dyn Provider>], name: &str) -> Result<usize> {
    providers
        .iter()
        .position(|p| p.name() == name)
        .with_context(|| format!("no provider named '{name}'"))
}

// Providers read again from disk, the named one still has to be among them
fn reloaded(name: &str) -> Result<Vec<Box<dyn Provider>>> {
    let providers = providers()?;
    position(&providers, name)?;
    Ok(providers)
}

// store a profile from knob values instead of a login, eg claude behind a proxy
fn cmd_configure(args: &[String], wait: Option<Wait>) -> Result<()> {
    let provider = args
        .first()
        .context("usage: accio configure <provider> [name] KEY=VALUE...")?;
    let mut providers = providers()?;
    let mut pi = position(&providers, provider)?;
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
    // Rewriting the active profile changes the login live sessions run on
    let p = &providers[pi];
    let live = p.active().filter(|&i| {
        let active = p.accounts().get(i).map(|a| a.name.clone());
        name.map(sanitize_name) == active
    });
    if let Some(idx) = live {
        match gate(&**p, Some(idx), "rewriting the active profile", wait)? {
            Gate::Clear => {}
            Gate::Cancelled => {
                println!("configure cancelled");
                return Ok(());
            }
            Gate::Waited => {
                providers = reloaded(provider)?;
                pi = position(&providers, provider)?;
            }
        }
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

// definitions come and go on disk, built ins only ever switch off
fn cmd_provider(args: &[String]) -> Result<()> {
    let registry = Registry::load()?;
    let name = args.get(1).map(String::as_str);
    match (args.first().map(String::as_str), name) {
        (None | Some("list"), _) => {
            for d in &registry.definitions {
                let line = format!(
                    "{} {:<8} {:<8} {}",
                    if d.disabled { "-" } else { "*" },
                    d.name,
                    if d.builtin { "built in" } else { "custom" },
                    if d.builtin {
                        String::new()
                    } else {
                        d.path.display().to_string()
                    }
                );
                println!("{}", line.trim_end());
            }
        }
        (Some("add"), Some(name)) => {
            let path = match (args.get(2).map(String::as_str), args.get(3)) {
                (Some("--preset"), Some(preset)) => registry.add_from_preset(preset, name)?,
                (Some("--preset"), None) => bail!("usage: accio provider add <name> --preset <preset>"),
                (command, _) => registry.add(name, command)?,
            };
            println!("defined '{name}' in {}\nedit it, then `accio configure {name}`", path.display());
        }
        (Some("presets"), _) => {
            for p in registry.presets()? {
                let from = match p.path {
                    Some(path) => path.display().to_string(),
                    None => "bundled".to_string(),
                };
                println!("{:<8} {from}", p.name);
            }
            println!("drop your own in {}", registry.presets_dir().display());
        }
        (Some("remove"), Some(name)) => {
            registry.remove(name)?;
            let profiles = dirs::config_dir()
                .context("cant determine config dir")?
                .join("accio")
                .join("accounts")
                .join(name);
            println!("removed '{name}' - its profiles stay in {}", profiles.display());
        }
        (Some("enable"), Some(name)) => {
            registry.set_disabled(name, false)?;
            println!("enabled '{name}'");
        }
        (Some("disable"), Some(name)) => {
            registry.set_disabled(name, true)?;
            println!("disabled '{name}'");
        }
        _ => bail!(
            "usage: accio provider [list | presets | add <name> [command | --preset <preset>] | remove <name> | enable <name> | disable <name>]"
        ),
    }
    Ok(())
}

fn print_help() {
    println!(
        "accio - switch between provider accounts without re-auth

usage:
  accio                                           open the TUI
  accio <provider> [--profile NAME] [--] [ARGS...] launch a harness with session-only credentials
  accio <your other account>                      make account the live account (provider/name if ambiguous)
  accio ... --wait turns|close                    switch, add or configure once live sessions end their turns or close
  accio ... --now                                 switch, add or configure without waiting on live sessions
  accio list                                      list accounts
  accio add [provider] [name]                     add currently logged in account or log one in if specified
  accio configure <provider> [name] KEY=VALUE...  for advance profile configuration
  accio remove <your other account>               remove an account from accio
  accio provider list                             built in and custom providers
  accio provider presets                          bundled definition templates
  accio provider add <name> [command]             define a custom provider, then edit the file it prints
  accio provider add <name> --preset <preset>     define one from a preset
  accio provider remove <name>                    drop a custom provider, its profiles stay on disk
  accio provider enable|disable <name>            show or hide a provider
  accio help                                      show this help"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wait_flags_leave_the_command_line_and_numbers_pick_sessions_to_outlast() {
        let args = |line: &str| line.split(' ').map(str::to_string).collect::<Vec<_>>();
        let mut switch = args("work --wait turns");
        assert_eq!(take_wait(&mut switch).unwrap(), Some(Wait::Turn));
        assert_eq!(switch, ["work"]);
        let mut configure = args("configure --wait close claude glm KEY=--now --now");
        assert_eq!(take_wait(&mut configure).unwrap(), Some(Wait::Now));
        assert_eq!(configure, ["configure", "claude", "glm", "KEY=--now"]);
        let mut plain = args("add claude work");
        assert_eq!(take_wait(&mut plain).unwrap(), None);
        assert_eq!(plain.len(), 3);
        assert!(take_wait(&mut args("work --wait")).is_err());
        assert!(take_wait(&mut args("work --wait soon")).is_err());

        assert_eq!(
            picked("1, 3", 3),
            Some(vec![Wait::Close, Wait::Now, Wait::Close])
        );
        assert_eq!(picked("2", 2), Some(vec![Wait::Now, Wait::Close]));
        assert_eq!(picked("0", 2), None);
        assert_eq!(picked("3", 2), None);
        assert_eq!(picked("one", 2), None);
    }
}
