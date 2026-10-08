//! Every scalar a provider returns, as a metric with a stable id
use std::cmp::Ordering;

use chrono::{DateTime, Local, Utc};
use serde_json::{Map, Value};

// How far a number can go, which decides how its bar fills
#[derive(Clone, Debug, PartialEq)]
pub enum Scale {
    Percent,
    Limit(f64),
    Relative,
}

#[derive(Clone, Debug)]
pub enum MetricValue {
    Number { value: f64, scale: Scale },
    Time(DateTime<Utc>),
    Bool(bool),
    Text(String),
}

// A sibling time that ends the counted period
#[derive(Clone, Debug)]
pub struct Until {
    pub key: String,
    pub at: DateTime<Utc>,
}

#[derive(Clone, Debug)]
pub struct Metric {
    pub id: String,
    pub path: Vec<String>,
    pub value: MetricValue,
    pub until: Option<Until>,
}

#[derive(Clone, Debug, Default)]
pub struct Usage {
    pub metrics: Vec<Metric>,
}

// Walks the whole response in its own order and keeps every leaf
pub fn parse_usage(v: &Value) -> Usage {
    let mut walker = Walker::default();
    walker.walk(v, &mut Vec::new());
    Usage {
        metrics: walker.metrics,
    }
}

impl MetricValue {
    pub fn is_number(&self) -> bool {
        matches!(self, MetricValue::Number { .. })
    }

    // Fraction of the bar to fill, relative numbers need the largest value they sit beside
    pub fn fill(&self, relative_max: f64) -> Option<f64> {
        let MetricValue::Number { value, scale } = self else {
            return None;
        };
        let max = match scale {
            Scale::Percent => 100.0,
            Scale::Limit(limit) => *limit,
            Scale::Relative => relative_max,
        };
        Some(if max > 0.0 { value / max } else { 0.0 })
    }

    pub fn text(&self) -> String {
        match self {
            MetricValue::Number {
                value,
                scale: Scale::Percent,
            } => format_percent(*value),
            MetricValue::Number { value, .. } => format_number(*value),
            MetricValue::Time(t) => format!(
                "{} · {}",
                humanize_when(*t),
                t.with_timezone(&Local).format("%b %-d, %H:%M")
            ),
            MetricValue::Bool(true) => "yes".to_string(),
            MetricValue::Bool(false) => "no".to_string(),
            MetricValue::Text(s) => s.clone(),
        }
    }

    // Shortest honest rendering for a one line summary
    pub fn brief(&self) -> String {
        match self {
            MetricValue::Time(t) => humanize_when(*t),
            other => other.text(),
        }
    }

    pub fn compare(&self, other: &MetricValue) -> Ordering {
        match (self, other) {
            (MetricValue::Number { value: a, .. }, MetricValue::Number { value: b, .. }) => {
                a.total_cmp(b)
            }
            (MetricValue::Time(a), MetricValue::Time(b)) => a.cmp(b),
            (MetricValue::Bool(a), MetricValue::Bool(b)) => a.cmp(b),
            (MetricValue::Text(a), MetricValue::Text(b)) => a.to_lowercase().cmp(&b.to_lowercase()),
            _ => self.kind_rank().cmp(&other.kind_rank()),
        }
    }

    fn kind_rank(&self) -> u8 {
        match self {
            MetricValue::Number { .. } => 0,
            MetricValue::Time(_) => 1,
            MetricValue::Bool(_) => 2,
            MetricValue::Text(_) => 3,
        }
    }
}

// Label for a path once the segments shared by every metric are dropped
pub fn label(path: &[String], skip: usize) -> String {
    path[skip..]
        .iter()
        .map(|k| humanize(k))
        .collect::<Vec<_>>()
        .join(" · ")
}

// Count of leading segments every path shares while each keeps its last two
pub fn common_prefix<'a>(paths: impl Iterator<Item = &'a [String]> + Clone) -> usize {
    let Some(first) = paths.clone().next() else {
        return 0;
    };
    let mut shared = first.len().saturating_sub(2);
    for path in paths {
        shared = shared.min(path.len().saturating_sub(2));
        shared = shared.min(
            first
                .iter()
                .zip(path.iter())
                .take_while(|(a, b)| a == b)
                .count(),
        );
    }
    shared
}

#[derive(Default)]
struct Walker {
    metrics: Vec<Metric>,
    omit: Vec<Vec<String>>,
}

impl Walker {
    fn walk(&mut self, v: &Value, path: &mut Vec<String>) {
        match v {
            Value::Object(map) => self.object(map, path),
            Value::Array(items) => self.array(items, path),
            Value::Null => {}
            scalar => self.leaf(scalar, path, None, None),
        }
    }

    fn object(&mut self, map: &Map<String, Value>, path: &mut Vec<String>) {
        let limit = map
            .iter()
            .find(|(k, v)| is_limit_key(k) && v.as_f64().is_some_and(|n| n > 0.0))
            .map(|(k, v)| (k.as_str(), v.as_f64().unwrap_or(0.0)));
        let until = map
            .iter()
            .filter(|(k, _)| is_until_key(k))
            .find_map(|(k, v)| time_of(k, v).map(|at| Until { key: k.clone(), at }));
        for (k, child) in map {
            if child.is_string() && is_sensitive(k) {
                continue;
            }
            path.push(k.clone());
            match child {
                Value::Object(m) => self.object(m, path),
                Value::Array(items) => self.array(items, path),
                Value::Null => {}
                scalar => {
                    let limit = limit.filter(|(lk, _)| *lk != k).map(|(_, l)| l);
                    self.leaf(scalar, path, limit, until.as_ref());
                }
            }
            path.pop();
        }
    }

    fn array(&mut self, items: &[Value], path: &mut Vec<String>) {
        for ((key, id_path), item) in element_keys(items).into_iter().zip(items) {
            path.push(key);
            if let Some(rel) = id_path {
                let mut absolute = path.clone();
                absolute.extend(rel);
                self.omit.push(absolute);
            }
            self.walk(item, path);
            path.pop();
        }
    }

    fn leaf(&mut self, v: &Value, path: &[String], limit: Option<f64>, until: Option<&Until>) {
        let Some(key) = path.last() else { return };
        if self.omit.iter().any(|o| o == path) {
            return;
        }
        let value = match v {
            Value::Bool(b) => MetricValue::Bool(*b),
            Value::Number(n) => match (time_of(key, v), n.as_f64()) {
                (Some(t), _) => MetricValue::Time(t),
                (None, Some(value)) => MetricValue::Number {
                    value,
                    scale: scale_of(key, limit),
                },
                (None, None) => return,
            },
            Value::String(s) => {
                let s = s.trim();
                if s.is_empty() {
                    return;
                }
                if let Some(t) = time_of(key, v) {
                    MetricValue::Time(t)
                } else if let Some(p) = percent_string(s) {
                    MetricValue::Number {
                        value: p,
                        scale: Scale::Percent,
                    }
                } else {
                    MetricValue::Text(s.to_string())
                }
            }
            _ => return,
        };
        let until = until.filter(|_| value.is_number()).cloned();
        self.metrics.push(Metric {
            id: path.join("."),
            path: path.to_vec(),
            value,
            until,
        });
    }
}

// Name array items by a string that tells them apart so ids survive reordering
fn element_keys(items: &[Value]) -> Vec<(String, Option<Vec<String>>)> {
    let indexes = || (0..items.len()).map(|i| (i.to_string(), None)).collect();
    let Some(first) = items.first() else {
        return indexes();
    };
    if !items.iter().all(Value::is_object) {
        return indexes();
    }
    let mut candidates = Vec::new();
    string_paths(first, &mut Vec::new(), &mut candidates);
    candidates.sort_by_key(|p| (!p.last().is_some_and(|k| is_identity_key(k)), p.len()));
    for candidate in candidates {
        let identity = candidate.last().is_some_and(|k| is_identity_key(k));
        if !identity && items.len() < 2 {
            continue;
        }
        let values: Vec<&str> = items
            .iter()
            .filter_map(|item| pointer(item, &candidate).and_then(Value::as_str))
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect();
        let distinct = values
            .iter()
            .enumerate()
            .all(|(i, v)| !values[..i].contains(v));
        if values.len() == items.len() && distinct {
            return values
                .into_iter()
                .map(|v| (v.to_string(), Some(candidate.clone())))
                .collect();
        }
    }
    indexes()
}

fn string_paths(v: &Value, path: &mut Vec<String>, out: &mut Vec<Vec<String>>) {
    match v {
        Value::Object(map) => {
            for (k, child) in map {
                if child.is_string() && is_sensitive(k) {
                    continue;
                }
                path.push(k.clone());
                string_paths(child, path, out);
                path.pop();
            }
        }
        Value::String(_) if !path.is_empty() => out.push(path.clone()),
        _ => {}
    }
}

fn pointer<'a>(v: &'a Value, path: &[String]) -> Option<&'a Value> {
    path.iter().try_fold(v, |cur, k| cur.get(k))
}

fn scale_of(key: &str, limit: Option<f64>) -> Scale {
    if is_percent_key(key) {
        Scale::Percent
    } else if let Some(limit) = limit {
        Scale::Limit(limit)
    } else {
        Scale::Relative
    }
}

fn is_percent_key(k: &str) -> bool {
    let k = k.to_ascii_lowercase();
    k.contains("percent") || k.contains("utilization") || k.contains("pct")
}

fn is_limit_key(k: &str) -> bool {
    let k = k.to_ascii_lowercase();
    [
        "limit",
        "max",
        "total",
        "cap",
        "quota",
        "allowance",
        "budget",
    ]
    .iter()
    .any(|w| k.contains(w))
}

fn is_until_key(k: &str) -> bool {
    let k = k.to_ascii_lowercase();
    k.contains("reset") || k.contains("expir")
}

fn is_time_key(k: &str) -> bool {
    let k = k.to_ascii_lowercase();
    k.ends_with("at")
        || ["time", "date", "reset", "expir", "epoch"]
            .iter()
            .any(|w| k.contains(w))
}

fn is_identity_key(k: &str) -> bool {
    let k = humanize(k);
    [
        "id",
        "kind",
        "name",
        "label",
        "type",
        "key",
        "slug",
        "title",
        "model",
        "window",
        "bucket",
        "period",
        "scope",
        "display name",
        "tier",
        "plan",
    ]
    .contains(&k.as_str())
}

fn is_sensitive(k: &str) -> bool {
    let k = k.to_ascii_lowercase();
    ["token", "secret", "password", "credential", "authorization"]
        .iter()
        .any(|bad| k.contains(bad))
}

fn percent_string(s: &str) -> Option<f64> {
    s.strip_suffix('%')?.trim().parse().ok()
}

// Rfc3339 strings always, raw integers only under a time named key and of epoch size
fn time_of(key: &str, v: &Value) -> Option<DateTime<Utc>> {
    if let Some(s) = v.as_str() {
        return DateTime::parse_from_rfc3339(s.trim())
            .ok()
            .map(|t| t.with_timezone(&Utc));
    }
    if !is_time_key(key) {
        return None;
    }
    let n = v.as_i64()?;
    match n {
        1_000_000_000..=99_999_999_999 => DateTime::from_timestamp(n, 0),
        1_000_000_000_000..=99_999_999_999_999 => DateTime::from_timestamp_millis(n),
        _ => None,
    }
}

// snake_case, kebab-case and camelCase all read as words
fn humanize(k: &str) -> String {
    let mut out = String::with_capacity(k.len() + 4);
    let mut prev_lower = false;
    for c in k.chars() {
        if c == '_' || c == '-' {
            out.push(' ');
            prev_lower = false;
        } else if c.is_uppercase() {
            if prev_lower {
                out.push(' ');
            }
            out.extend(c.to_lowercase());
            prev_lower = false;
        } else {
            out.push(c);
            prev_lower = c.is_lowercase();
        }
    }
    out
}

fn format_percent(p: f64) -> String {
    if (p - p.round()).abs() < 0.05 {
        format!("{p:.0}%")
    } else {
        format!("{p:.1}%")
    }
}

fn format_number(n: f64) -> String {
    if n.fract() != 0.0 || n.abs() >= 1e15 {
        return format!("{n:.2}")
            .trim_end_matches('0')
            .trim_end_matches('.')
            .to_string();
    }
    let digits = format!("{}", (n as i64).abs());
    let mut grouped = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(c);
    }
    if n < 0.0 {
        grouped.insert(0, '-');
    }
    grouped
}

fn humanize_span(secs: i64) -> String {
    let (d, h, m) = (secs / 86_400, (secs % 86_400) / 3_600, (secs % 3_600) / 60);
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else {
        format!("{m}m")
    }
}

pub fn humanize_until(t: DateTime<Utc>) -> String {
    let secs = (t - Utc::now()).num_seconds();
    if secs <= 0 {
        return "now".to_string();
    }
    humanize_span(secs)
}

pub fn humanize_when(t: DateTime<Utc>) -> String {
    let secs = (t - Utc::now()).num_seconds();
    if secs.abs() < 60 {
        "now".to_string()
    } else if secs > 0 {
        format!("in {}", humanize_span(secs))
    } else {
        format!("{} ago", humanize_span(-secs))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find<'a>(u: &'a Usage, id: &str) -> &'a Metric {
        u.metrics
            .iter()
            .find(|m| m.id == id)
            .unwrap_or_else(|| panic!("no metric {id} in {:?}", ids(u)))
    }

    fn ids(u: &Usage) -> Vec<&str> {
        u.metrics.iter().map(|m| m.id.as_str()).collect()
    }

    fn number(m: &Metric) -> (f64, Scale) {
        match &m.value {
            MetricValue::Number { value, scale } => (*value, scale.clone()),
            other => panic!("{} is not a number: {other:?}", m.id),
        }
    }

    #[test]
    fn array_items_are_named_by_their_identity_string() {
        let v: Value = serde_json::json!({
            "five_hour": {"utilization": 5.0, "resets_at": "2026-08-03T00:29:59+00:00"},
            "limits": [
                {"kind": "session", "percent": 5, "severity": "normal", "resets_at": "2026-08-03T00:29:59+00:00"},
                {"kind": "weekly_all", "percent": 42, "severity": "normal", "resets_at": null},
                {"kind": "weekly_scoped", "percent": 78, "severity": "warning",
                 "scope": {"model": {"display_name": "Fable"}}}
            ]
        });
        let u = parse_usage(&v);
        assert_eq!(
            ids(&u),
            [
                "five_hour.utilization",
                "five_hour.resets_at",
                "limits.session.percent",
                "limits.session.severity",
                "limits.session.resets_at",
                "limits.weekly_all.percent",
                "limits.weekly_all.severity",
                "limits.weekly_scoped.percent",
                "limits.weekly_scoped.severity",
                "limits.weekly_scoped.scope.model.display_name",
            ]
        );
        assert_eq!(
            number(find(&u, "limits.weekly_scoped.percent")),
            (78.0, Scale::Percent)
        );
        let session = find(&u, "limits.session.percent");
        assert_eq!(session.until.as_ref().unwrap().key, "resets_at");
        assert!(find(&u, "limits.weekly_all.percent").until.is_none());
        assert!(matches!(
            &find(&u, "limits.session.resets_at").value,
            MetricValue::Time(_)
        ));
        assert!(matches!(
            &find(&u, "limits.weekly_scoped.severity").value,
            MetricValue::Text(s) if s == "warning"
        ));
    }

    #[test]
    fn array_items_fall_back_to_index_without_a_telling_string() {
        let v: Value = serde_json::json!({
            "rows": [
                {"severity": "normal", "percent": 1},
                {"severity": "normal", "percent": 2}
            ],
            "single": [{"severity": "normal", "used": 3}],
            "models": [
                {"used": 1, "info": {"model": "opus"}},
                {"used": 2, "info": {"model": "sonnet"}}
            ],
            "unique_strings": [{"region": "us", "n": 1}, {"region": "eu", "n": 2}]
        });
        let u = parse_usage(&v);
        assert_eq!(number(find(&u, "rows.0.percent")).0, 1.0);
        assert_eq!(number(find(&u, "rows.1.percent")).0, 2.0);
        assert!(matches!(
            &find(&u, "rows.0.severity").value,
            MetricValue::Text(_)
        ));
        assert_eq!(number(find(&u, "single.0.used")).0, 3.0);
        assert_eq!(number(find(&u, "models.sonnet.used")).0, 2.0);
        assert!(!ids(&u).contains(&"models.sonnet.info.model"));
        assert_eq!(number(find(&u, "unique_strings.eu.n")).0, 2.0);
    }

    #[test]
    fn numbers_take_their_scale_from_the_key_or_a_limit_beside_them() {
        let v: Value = serde_json::json!({
            "extra_usage": {"used_credits": 12.5, "monthly_limit": 50, "is_enabled": false},
            "seats": 3,
            "ratio": "12.5%",
            "rows": 1700000000,
            "updated_at": 1700000000,
            "expires_at": 1700000000000_i64,
            "tokens_used": 40,
            "access_token": "sk-should-not-show",
            "organization": {"tier": "max"}
        });
        let u = parse_usage(&v);
        assert_eq!(
            number(find(&u, "extra_usage.used_credits")),
            (12.5, Scale::Limit(50.0))
        );
        assert_eq!(
            number(find(&u, "extra_usage.monthly_limit")),
            (50.0, Scale::Relative)
        );
        assert!(matches!(
            find(&u, "extra_usage.is_enabled").value,
            MetricValue::Bool(false)
        ));
        assert_eq!(number(find(&u, "seats")), (3.0, Scale::Relative));
        assert_eq!(number(find(&u, "ratio")), (12.5, Scale::Percent));
        assert_eq!(number(find(&u, "rows")), (1_700_000_000.0, Scale::Relative));
        assert!(matches!(find(&u, "updated_at").value, MetricValue::Time(_)));
        assert!(matches!(find(&u, "expires_at").value, MetricValue::Time(_)));
        assert_eq!(number(find(&u, "tokens_used")).0, 40.0);
        assert!(!ids(&u).contains(&"access_token"));
        assert!(matches!(&find(&u, "organization.tier").value, MetricValue::Text(s) if s == "max"));
        assert_eq!(find(&u, "rows").value.text(), "1,700,000,000");
        assert_eq!(find(&u, "ratio").value.text(), "12.5%");
        assert_eq!(
            find(&u, "extra_usage.used_credits").value.fill(0.0),
            Some(0.25)
        );
        assert_eq!(find(&u, "seats").value.fill(12.0), Some(0.25));
        assert_eq!(find(&u, "seats").value.fill(0.0), Some(0.0));
    }

    #[test]
    fn values_order_within_their_kind_and_numbers_come_first() {
        let n = |v: f64| MetricValue::Number {
            value: v,
            scale: Scale::Relative,
        };
        assert_eq!(n(1.0).compare(&n(2.0)), Ordering::Less);
        assert_eq!(
            MetricValue::Text("b".into()).compare(&MetricValue::Text("A".into())),
            Ordering::Greater
        );
        assert_eq!(
            n(5.0).compare(&MetricValue::Text("x".into())),
            Ordering::Less
        );
        assert_eq!(
            MetricValue::Bool(false).compare(&MetricValue::Bool(true)),
            Ordering::Less
        );
    }

    #[test]
    fn labels_drop_the_shared_prefix_and_read_as_words() {
        let paths: Vec<Vec<String>> = [
            vec!["limits", "session", "percentUsed"],
            vec!["limits", "weekly_all", "percent"],
        ]
        .iter()
        .map(|p| p.iter().map(|s| s.to_string()).collect())
        .collect();
        let skip = common_prefix(paths.iter().map(Vec::as_slice));
        assert_eq!(skip, 1);
        assert_eq!(label(&paths[0], skip), "session · percent used");
        let lone: Vec<Vec<String>> = vec![vec!["auth".into(), "plan".into()], vec!["auth".into()]];
        assert_eq!(common_prefix(lone.iter().map(Vec::as_slice)), 0);
        assert_eq!(common_prefix(paths[..1].iter().map(Vec::as_slice)), 1);
        assert_eq!(label(&paths[0], 1), "session · percent used");
        assert_eq!(common_prefix(std::iter::empty()), 0);
        assert_eq!(humanize("displayName"), "display name");
        assert_eq!(humanize("five-hour_WINDOW"), "five hour window");
    }

    #[test]
    fn unknown_shape_yields_empty_not_panic() {
        assert!(parse_usage(&serde_json::json!([1, 2])).metrics.len() == 2);
        assert!(parse_usage(&serde_json::json!(3)).metrics.is_empty());
        assert!(
            parse_usage(&serde_json::json!({"a": null, "b": "", "c": {}, "d": []}))
                .metrics
                .is_empty()
        );
    }
}
