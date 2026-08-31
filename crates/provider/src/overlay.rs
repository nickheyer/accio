//! Profile entries laid over the live surface - literal files and json merges

use std::collections::{BTreeMap, BTreeSet};
use std::fs;

use anyhow::{Context, Result};
use serde_json::{Map, Value};

use crate::fsutil::{expand, read_json, write_atomic};

const MERGE: &str = "merge:";

// Path-shaped and merge keys, the rest is backend business
pub fn is_overlay_key(k: &str) -> bool {
    k.starts_with("~/") || k.starts_with('/') || k.starts_with(MERGE)
}

pub fn is_merge_key(k: &str) -> bool {
    k.starts_with(MERGE)
}

pub fn split(
    bag: &BTreeMap<String, String>,
) -> (BTreeMap<String, String>, BTreeMap<String, String>) {
    let (mut overlays, mut plain) = (BTreeMap::new(), BTreeMap::new());
    for (k, v) in bag {
        if is_overlay_key(k) {
            overlays.insert(k.clone(), v.clone());
        } else {
            plain.insert(k.clone(), v.clone());
        }
    }
    (overlays, plain)
}

// Every path and merge leaf any profile lays claim to
#[derive(Default)]
pub struct Spec {
    paths: BTreeSet<String>,
    merges: BTreeMap<String, BTreeSet<Vec<String>>>,
}

impl Spec {
    pub fn add(&mut self, files: &BTreeMap<String, String>) {
        for (k, content) in files {
            if let Some(path) = k.strip_prefix(MERGE) {
                let chains = self.merges.entry(path.to_string()).or_default();
                if let Ok(v) = serde_json::from_str::<Value>(content) {
                    collect_chains(&v, &mut Vec::new(), chains);
                }
            } else if is_overlay_key(k) {
                self.paths.insert(k.clone());
            }
        }
    }
}

// What of the owned surface is on disk right now
pub fn read(spec: &Spec) -> BTreeMap<String, String> {
    let mut live = BTreeMap::new();
    for key in &spec.paths {
        if let Ok(content) = fs::read_to_string(expand(key)) {
            live.insert(key.clone(), content);
        }
    }
    for (mpath, chains) in &spec.merges {
        let doc = match read_json(&expand(mpath)) {
            Some(d) => d,
            None => continue,
        };
        let mut owned = Value::Object(Map::new());
        for chain in chains {
            if let Some(v) = get_chain(&doc, chain) {
                set_chain(&mut owned, chain, v.clone());
            }
        }
        if owned.as_object().is_some_and(|m| !m.is_empty()) {
            live.insert(format!("{MERGE}{mpath}"), canon(&owned).to_string());
        }
    }
    live
}

// Clear everything the spec owns then lay down the incoming entries
pub fn write(spec: &Spec, incoming: &BTreeMap<String, String>) -> Result<()> {
    for key in &spec.paths {
        let path = expand(key);
        match incoming.get(key) {
            Some(content) => {
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)
                        .with_context(|| format!("cant create {}", parent.display()))?;
                }
                write_atomic(&path, content.as_bytes())?;
            }
            None if path.exists() => {
                fs::remove_file(&path)
                    .with_context(|| format!("cant remove {}", path.display()))?;
            }
            None => {}
        }
    }
    for (mpath, chains) in &spec.merges {
        let path = expand(mpath);
        let existed = path.exists();
        let mut doc = read_json(&path).unwrap_or_else(|| Value::Object(Map::new()));
        let before = doc.clone();
        for chain in chains {
            remove_chain(&mut doc, chain);
        }
        if let Some(content) = incoming.get(&format!("{MERGE}{mpath}")) {
            let patch: Value = serde_json::from_str(content)
                .with_context(|| format!("bad merge content for {mpath}"))?;
            deep_merge(&mut doc, &patch);
        }
        if (existed && doc == before) || (!existed && doc.as_object().is_some_and(Map::is_empty)) {
            continue;
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("cant create {}", parent.display()))?;
        }
        write_atomic(&path, serde_json::to_string_pretty(&doc)?.as_bytes())?;
    }
    Ok(())
}

// Sorted keys so equal docs compare equal as strings
pub fn canon(v: &Value) -> Value {
    match v {
        Value::Object(map) => {
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            entries.sort_by_key(|(k, _)| k.as_str());
            Value::Object(
                entries
                    .into_iter()
                    .map(|(k, v)| (k.clone(), canon(v)))
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(items.iter().map(canon).collect()),
        other => other.clone(),
    }
}

pub fn canon_str(content: &str) -> String {
    match serde_json::from_str::<Value>(content) {
        Ok(v) => canon(&v).to_string(),
        Err(_) => content.to_string(),
    }
}

fn collect_chains(v: &Value, path: &mut Vec<String>, out: &mut BTreeSet<Vec<String>>) {
    match v {
        Value::Object(map) if !map.is_empty() => {
            for (k, child) in map {
                path.push(k.clone());
                collect_chains(child, path, out);
                path.pop();
            }
        }
        _ => {
            if !path.is_empty() {
                out.insert(path.clone());
            }
        }
    }
}

fn get_chain<'a>(doc: &'a Value, chain: &[String]) -> Option<&'a Value> {
    let mut cur = doc;
    for k in chain {
        cur = cur.as_object()?.get(k)?;
    }
    Some(cur)
}

fn set_chain(doc: &mut Value, chain: &[String], val: Value) {
    let (leaf, parents) = match chain.split_last() {
        Some(s) => s,
        None => return,
    };
    let mut cur = doc;
    for k in parents {
        if !cur.is_object() {
            *cur = Value::Object(Map::new());
        }
        cur = cur
            .as_object_mut()
            .unwrap()
            .entry(k.clone())
            .or_insert_with(|| Value::Object(Map::new()));
    }
    if !cur.is_object() {
        *cur = Value::Object(Map::new());
    }
    cur.as_object_mut().unwrap().insert(leaf.clone(), val);
}

// Remove the leaf then prune parents it emptied
fn remove_chain(doc: &mut Value, chain: &[String]) {
    let Some(map) = doc.as_object_mut() else {
        return;
    };
    match chain {
        [] => {}
        [leaf] => {
            map.remove(leaf);
        }
        [head, rest @ ..] => {
            if let Some(child) = map.get_mut(head) {
                remove_chain(child, rest);
                if child.as_object().is_some_and(Map::is_empty) {
                    map.remove(head);
                }
            }
        }
    }
}

fn deep_merge(dst: &mut Value, src: &Value) {
    match (dst, src) {
        (Value::Object(d), Value::Object(s)) => {
            for (k, v) in s {
                deep_merge(d.entry(k.clone()).or_insert(Value::Null), v);
            }
        }
        (d, s) => *d = s.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tdir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("accio-overlay-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn merge_swaps_in_and_out_leaving_user_config() {
        let dir = tdir("merge");
        let settings = dir.join("settings.json");
        fs::write(&settings, r#"{"theme":"dark","env":{"MINE":"keep"}}"#).unwrap();
        let key = format!("merge:{}", settings.display());
        let mut bag = BTreeMap::new();
        bag.insert(
            key.clone(),
            r#"{"env":{"X_BASE_URL":"https://a.example"}}"#.to_string(),
        );
        let mut spec = Spec::default();
        spec.add(&bag);

        write(&spec, &bag).unwrap();
        let live = read(&spec);
        assert_eq!(live.get(&key).unwrap(), &canon_str(&bag[&key]));
        let doc = read_json(&settings).unwrap();
        assert_eq!(doc.pointer("/env/MINE").unwrap(), "keep");
        assert_eq!(doc.pointer("/theme").unwrap(), "dark");

        write(&spec, &BTreeMap::new()).unwrap();
        assert!(read(&spec).is_empty());
        let doc = read_json(&settings).unwrap();
        assert_eq!(doc.pointer("/env/MINE").unwrap(), "keep");
        assert!(doc.pointer("/env/X_BASE_URL").is_none());
    }

    #[test]
    fn path_entries_come_and_go() {
        let dir = tdir("path");
        let key = dir.join("thing.env").display().to_string();
        let mut bag = BTreeMap::new();
        bag.insert(key.clone(), "A=1\n".to_string());
        let mut spec = Spec::default();
        spec.add(&bag);
        write(&spec, &bag).unwrap();
        assert_eq!(read(&spec).get(&key).unwrap(), "A=1\n");
        write(&spec, &BTreeMap::new()).unwrap();
        assert!(read(&spec).is_empty());
    }

    #[test]
    fn canon_orders_keys() {
        assert_eq!(
            canon_str(r#"{"b":1,"a":{"d":[2],"c":3}}"#),
            r#"{"a":{"c":3,"d":[2]},"b":1}"#
        );
    }

    #[test]
    fn prunes_emptied_parents_only() {
        let mut doc: Value = serde_json::from_str(r#"{"env":{"X":1,"Y":2},"keep":true}"#).unwrap();
        remove_chain(&mut doc, &["env".into(), "X".into()]);
        assert!(doc.pointer("/env/Y").is_some());
        remove_chain(&mut doc, &["env".into(), "Y".into()]);
        assert!(doc.get("env").is_none());
        assert!(doc.get("keep").is_some());
    }
}
