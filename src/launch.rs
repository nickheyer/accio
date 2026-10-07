use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use accio_provider::Provider;
use anyhow::{Context, Result};

pub fn run(name: &str, args: &[OsString]) -> Result<()> {
    let (profile, args) = parse_args(args)?;
    let provider: Box<dyn Provider> = match name {
        "claude" => Box::new(accio_claude::session_provider()?),
        "codex" => Box::new(accio_codex::session_provider()?),
        "grok" => Box::new(accio_grok::session_provider()?),
        "gemini" => Box::new(accio_gemini::session_provider()?),
        _ => anyhow::bail!("unknown harness '{name}'"),
    };
    let (provider, idx) = match profile {
        Some(name) => {
            let idx = provider
                .accounts()
                .iter()
                .position(|a| a.name == name)
                .with_context(|| {
                    format!(
                        "no {} profile named '{name}' - see `accio list`",
                        provider.name()
                    )
                })?;
            (provider, idx)
        }
        None => match crate::tui::pick_session(provider)? {
            Some(selection) => selection,
            None => return Ok(()),
        },
    };

    let dir = session_dir(&session_root(name)?)?;
    let mut command = match provider.session(idx, &dir) {
        Ok(command) => command,
        Err(error) => {
            let _ = fs::remove_dir_all(&dir);
            return Err(error);
        }
    };
    command.args(args);
    // Replacing accio preserves the terminal, signals and the harness's exact exit status.
    // Keep the private directory so transcripts and refreshed credentials survive exit.
    use std::os::unix::process::CommandExt;
    let error = command.exec();
    let _ = fs::remove_dir_all(&dir);
    Err(error).with_context(|| format!("could not run `{name}` - is it on your PATH?"))
}

fn parse_args(args: &[OsString]) -> Result<(Option<&str>, &[OsString])> {
    let (profile, rest) = if args.first().is_some_and(|a| a == "--profile") {
        let name = args
            .get(1)
            .and_then(|a| a.to_str())
            .filter(|s| !s.is_empty() && !s.starts_with('-'))
            .context("usage: accio <provider> [--profile NAME] [--] [HARNESS_ARGS...]")?;
        (Some(name), &args[2..])
    } else {
        (None, args)
    };
    Ok((
        profile,
        if rest.first().is_some_and(|a| a == "--") {
            &rest[1..]
        } else {
            rest
        },
    ))
}

fn session_root(provider: &str) -> Result<PathBuf> {
    // Use the same platform config root as saved accio profiles.
    Ok(dirs::config_dir()
        .context("cant determine config dir")?
        .join("accio")
        .join("sessions")
        .join(provider))
}

fn session_dir(root: &Path) -> Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;
    fs::create_dir_all(root)?;
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    for attempt in 0..100 {
        let dir = root.join(format!("{stamp}-{}-{attempt}", std::process::id()));
        match fs::DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).context("cant create private session directory"),
        }
    }
    anyhow::bail!("cant allocate a unique session directory")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_after_the_separator_are_passed_through() {
        let args = [
            "--profile",
            "work",
            "--",
            "-p",
            "a prompt with spaces",
            "--model",
            "sonnet",
        ]
        .map(OsString::from);
        let (profile, forwarded) = parse_args(&args).unwrap();
        assert_eq!(profile, Some("work"));
        assert_eq!(forwarded, &args[3..]);
        let args = ["--", "--profile", "literal"].map(OsString::from);
        assert_eq!(parse_args(&args).unwrap(), (None, &args[1..]));
        assert!(parse_args(&["--profile".into()]).is_err());
    }
}
