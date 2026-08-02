mod oauth;
mod store;
mod tui;

use anyhow::{Context, Result};
use store::Store;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        None => tui::run(),
        Some("list") => cmd_list(),
        Some("add") => cmd_add(args.get(1)),
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
    let store = Store::load()?;
    if store.profiles.is_empty() {
        println!("no accounts - run `accio add` or just `accio`");
        return Ok(());
    }
    for (i, p) in store.profiles.iter().enumerate() {
        let marker = if store.active == Some(i) { "*" } else { " " };
        println!(
            "{marker} {:<14} {:<30} {}",
            p.name,
            p.email().unwrap_or("-"),
            p.subscription().unwrap_or("")
        );
    }
    Ok(())
}

fn cmd_switch(name: &str) -> Result<()> {
    let mut store = Store::load()?;
    let idx = store
        .profiles
        .iter()
        .position(|p| p.name == name)
        .with_context(|| format!("no account named '{name}' - see `accio list`"))?;
    if store.active == Some(idx) {
        println!("'{name}' is already active");
        return Ok(());
    }
    store.activate(idx)?;
    println!("switched to '{name}'");
    Ok(())
}

fn cmd_add(name: Option<&String>) -> Result<()> {
    let mut store = Store::load()?;
    let msg = store.add_account(name.map(String::as_str))?;
    println!("{msg}");
    Ok(())
}

fn cmd_remove(name: Option<&String>) -> Result<()> {
    let name = name.context("usage: accio remove <name>")?;
    let mut store = Store::load()?;
    store.delete(name)?;
    println!("removed '{name}'");
    Ok(())
}

fn print_help() {
    println!(
        "accio - switch between anthropic accounts without re-auth

usage:
  accio                                open the TUI
  accio <your other account>           make account the live account
  accio list                           list accounts
  accio add [your other account]       add currently logged in account or log one in if specified
  accio remove <your other account>    remove an account from accio
  accio help                           show this help"
    );
}
