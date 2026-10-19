//! Utilities for building private harness configurations without touching live files.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use serde_json::{json, Value};

pub fn config_dir(variable: &str, default: &str) -> PathBuf {
    relocated(variable).unwrap_or_else(|| crate::fsutil::expand(default))
}

// The variable's directory unless empty or one accio itself handed to a session
pub fn relocated(variable: &str) -> Option<PathBuf> {
    let sessions = dirs::config_dir().map(|d| d.join("accio").join("sessions"));
    outside_sessions(
        std::env::var_os(variable).map(PathBuf::from),
        sessions.as_deref(),
    )
}

fn outside_sessions(dir: Option<PathBuf>, sessions: Option<&Path>) -> Option<PathBuf> {
    dir.filter(|d| !d.as_os_str().is_empty())
        .filter(|d| sessions.is_none_or(|s| !d.starts_with(s)))
}

pub fn read_object(path: &Path) -> Result<Value> {
    match fs::read(path) {
        Ok(bytes) => {
            let value: Value = serde_json::from_slice(&bytes)
                .with_context(|| format!("invalid JSON in {}", path.display()))?;
            anyhow::ensure!(
                value.is_object(),
                "expected an object in {}",
                path.display()
            );
            Ok(value)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(json!({})),
        Err(e) => Err(e).with_context(|| format!("cant read {}", path.display())),
    }
}

// Patch laid over the object at path, created when missing
pub fn merge_object(path: &Path, patch: &Value) -> Result<()> {
    anyhow::ensure!(
        patch.is_object(),
        "merge content for {} must be an object",
        path.display()
    );
    let mut doc = read_object(path)?;
    crate::overlay::deep_merge(&mut doc, patch);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("cant create {}", parent.display()))?;
    }
    crate::fsutil::write_atomic(path, serde_json::to_string_pretty(&doc)?.as_bytes())
}

// Every leaf the patch holds lifted out of the object at path, parents pruned
pub fn unmerge_object(path: &Path, patch: &Value) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let mut doc = read_object(path)?;
    let mut chains = BTreeSet::new();
    crate::overlay::collect_chains(patch, &mut Vec::new(), &mut chains);
    for chain in &chains {
        crate::overlay::remove_chain(&mut doc, chain);
    }
    crate::fsutil::write_atomic(path, serde_json::to_string_pretty(&doc)?.as_bytes())
}

pub fn copy_resources(source: &Path, target: &Path, names: &[&str]) -> Result<()> {
    for name in names {
        copy_resource(&source.join(name), &target.join(name), &mut BTreeSet::new())?;
    }
    Ok(())
}

// Materialize symlinks: a session must never write through them into the live config.
fn copy_resource(source: &Path, target: &Path, ancestors: &mut BTreeSet<PathBuf>) -> Result<()> {
    let canonical = match source.canonicalize() {
        Ok(path) => path,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("cant read {}", source.display())),
    };
    anyhow::ensure!(
        ancestors.insert(canonical.clone()),
        "symlink cycle in {}",
        source.display()
    );
    let metadata = fs::metadata(source)?;
    if metadata.is_dir() {
        fs::create_dir(target)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            copy_resource(&entry.path(), &target.join(entry.file_name()), ancestors)?;
        }
    } else if metadata.is_file() {
        fs::copy(source, target).with_context(|| format!("cant copy {}", source.display()))?;
    }
    ancestors.remove(&canonical);
    Ok(())
}

pub fn require_version(
    command: &mut Command,
    minimum: (u32, u32, u32),
    feature: &str,
) -> Result<()> {
    let program = command.get_program().to_string_lossy().into_owned();
    let out = command
        .arg("--version")
        .output()
        .with_context(|| format!("could not run `{program}` - is it on your PATH?"))?;
    let output = String::from_utf8_lossy(&out.stdout);
    let version = output.split_whitespace().find_map(|word| {
        let mut parts = word.trim_start_matches('v').split(['.', '-']);
        Some((
            parts.next()?.parse::<u32>().ok()?,
            parts.next()?.parse::<u32>().ok()?,
            parts.next()?.parse::<u32>().ok()?,
        ))
    });
    anyhow::ensure!(out.status.success() && version.is_some_and(|v| v >= minimum),
        "isolated {program} sessions require {program} {}.{}.{} or newer ({feature}); upgrade the harness first",
        minimum.0, minimum.1, minimum.2);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_directory_never_passes_for_the_live_config() {
        let sessions = Path::new("/cfg/accio/sessions");
        let inside = sessions.join("claude").join("123-4-0");
        assert_eq!(outside_sessions(Some(inside), Some(sessions)), None);
        let moved = PathBuf::from("/home/me/.claude-work");
        assert_eq!(
            outside_sessions(Some(moved.clone()), Some(sessions)),
            Some(moved.clone())
        );
        assert_eq!(outside_sessions(Some(moved.clone()), None), Some(moved));
        assert_eq!(outside_sessions(Some(PathBuf::new()), Some(sessions)), None);
        assert_eq!(outside_sessions(None, Some(sessions)), None);
    }
}
