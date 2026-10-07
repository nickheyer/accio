//! Utilities for building private harness configurations without touching live files.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use serde_json::{json, Value};

pub fn config_dir(variable: &str, default: &str) -> PathBuf {
    std::env::var_os(variable)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::fsutil::expand(default))
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
