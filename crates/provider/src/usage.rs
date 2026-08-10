//! Usage parsed structurally from whatever a provider returns
use chrono::{DateTime, Local, Utc};
use serde_json::{Map, Value};

#[derive(Clone, Debug)]
pub enum Severity {
    Normal,
    Warning,
    Exceeded,
}

#[derive(Clone, Debug)]
pub struct Window {
    pub label: String,
    pub percent: f64,
    pub resets_at: Option<DateTime<Utc>>,
    pub severity: Severity,
    graded: bool,
}

// arbitrary metrics - we take it all idc
#[derive(Clone, Debug)]
pub struct Fact {
    pub label: String,
    pub value: String,
}

#[derive(Clone, Debug, Default)]
pub struct Usage {
    pub windows: Vec<Window>,
    pub facts: Vec<Fact>,
}

// Walk the whole response and keep whatever we find
pub fn parse_usage(v: &Value) -> Usage {
    let mut usage = Usage::default();
    walk(v, &mut Vec::new(), &mut usage);
    usage.windows = merge_duplicates(usage.windows);
    usage.windows.sort_by(|a, b| {
        (a.resets_at.is_none(), a.resets_at, &a.label)
            .cmp(&(b.resets_at.is_none(), b.resets_at, &b.label))
    });
    usage
}

fn walk<'a>(v: &'a Value, path: &mut Vec<&'a str>, out: &mut Usage) {
    match v {
        Value::Object(map) => {
            if let Some(w) = (!path.is_empty()).then(|| window_from(map, path)).flatten() {
                out.windows.push(w);
                return;
            }
            for (k, child) in map {
                path.push(k.as_str());
                walk(child, path, out);
                path.pop();
            }
        }
        Value::Array(items) => items.iter().for_each(|i| walk(i, path, out)),
        Value::Null => {}
        scalar => {
            if let Some(f) = fact_from(scalar, path) {
                out.facts.push(f);
            }
        }
    }
}

fn window_from(map: &Map<String, Value>, path: &[&str]) -> Option<Window> {
    let percent = map
        .iter()
        .find(|(k, _)| is_percent_key(k.as_str()))
        .and_then(|(_, v)| v.as_f64())?;
    // the window names itself where it can, otherwise it is named by where it sits
    let named = map
        .iter()
        .find(|(k, _)| is_name_key(k.as_str()))
        .and_then(|(_, v)| v.as_str());
    let base = humanize(named.or_else(|| path.last().copied())?);
    let label = match map
        .iter()
        .filter(|(k, _)| !is_name_key(k.as_str()))
        .find_map(|(_, v)| nested_name(v))
    {
        Some(scope) => format!("{base} ({scope})"),
        None => base,
    };
    let severity = map
        .iter()
        .find(|(k, _)| k.contains("severity"))
        .and_then(|(_, v)| v.as_str());
    Some(Window {
        label,
        percent,
        resets_at: map
            .iter()
            .find(|(k, _)| k.contains("reset"))
            .and_then(|(_, v)| parse_time(Some(v))),
        severity: parse_severity(severity, percent),
        graded: severity.is_some(),
    })
}

fn fact_from(v: &Value, path: &[&str]) -> Option<Fact> {
    if path.is_empty() || path.iter().any(|k| is_sensitive(k)) {
        return None;
    }
    let value = match v {
        Value::Bool(b) => (if *b { "yes" } else { "no" }).to_string(),
        Value::Number(n) => format_number(n.as_f64()?),
        Value::String(s) if s.trim().is_empty() => return None,
        Value::String(s) => match parse_time(Some(v)) {
            Some(t) => t.with_timezone(&Local).format("%b %-d, %H:%M").to_string(),
            None => s.trim().to_string(),
        },
        _ => return None,
    };
    Some(Fact {
        label: path.iter().map(|k| humanize(k)).collect::<Vec<_>>().join(" · "),
        value,
    })
}

fn merge_duplicates(windows: Vec<Window>) -> Vec<Window> {
    let mut kept: Vec<Window> = Vec::new();
    for w in windows {
        match kept.iter().position(|k| same_window(k, &w)) {
            Some(i) if w.graded && !kept[i].graded => kept[i] = w,
            Some(_) => {}
            None => kept.push(w),
        }
    }
    kept
}

fn same_window(a: &Window, b: &Window) -> bool {
    (a.percent - b.percent).abs() < 0.01
        && a.resets_at == b.resets_at
        && (a.resets_at.is_some() || a.label == b.label)
}

fn is_percent_key(k: &str) -> bool {
    let k = k.to_ascii_lowercase();
    k.contains("percent") || k.contains("utilization") || k == "pct"
}

fn is_name_key(k: &str) -> bool {
    matches!(k, "kind" | "name" | "label")
}

fn is_sensitive(k: &str) -> bool {
    let k = k.to_ascii_lowercase();
    ["token", "secret", "password", "credential", "authorization"]
        .iter()
        .any(|bad| k.contains(bad))
}

// "Fable", "Opus", ...
fn nested_name(v: &Value) -> Option<String> {
    let map = v.as_object()?;
    map.iter()
        .find(|(k, v)| (*k == "display_name" || *k == "name") && v.is_string())
        .and_then(|(_, v)| v.as_str().map(str::to_string))
        .or_else(|| map.values().find_map(nested_name))
}

fn humanize(k: &str) -> String {
    k.replace('_', " ")
}

fn format_number(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        format!("{n:.2}")
    }
}

fn parse_severity(v: Option<&str>, percent: f64) -> Severity {
    match v {
        Some("normal") => Severity::Normal,
        Some("warning") => Severity::Warning,
        Some(_) => Severity::Exceeded,
        None if percent >= 90.0 => Severity::Exceeded,
        None if percent >= 70.0 => Severity::Warning,
        None => Severity::Normal,
    }
}

fn parse_time(v: Option<&Value>) -> Option<DateTime<Utc>> {
    let v = v?;
    if let Some(s) = v.as_str() {
        return DateTime::parse_from_rfc3339(s).ok().map(|t| t.with_timezone(&Utc));
    }
    v.as_i64().and_then(|secs| DateTime::from_timestamp(secs, 0))
}

pub fn humanize_until(t: DateTime<Utc>) -> String {
    let secs = (t - Utc::now()).num_seconds();
    if secs <= 0 {
        return "now".to_string();
    }
    let (d, h, m) = (secs / 86_400, (secs % 86_400) / 3_600, (secs % 3_600) / 60);
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else {
        format!("{m}m")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_limits_array() {
        let v: Value = serde_json::json!({
            "five_hour": {"utilization": 5.0, "resets_at": "2026-08-03T00:29:59+00:00"},
            "limits": [
                {"kind": "session", "percent": 5, "severity": "normal", "resets_at": "2026-08-03T00:29:59+00:00"},
                {"kind": "weekly_all", "percent": 42, "severity": "normal", "resets_at": null},
                {"kind": "weekly_scoped", "percent": 78, "severity": "warning",
                 "scope": {"model": {"display_name": "Fable"}}}
            ]
        });
        let w = parse_usage(&v).windows;
        assert_eq!(w.len(), 3);
        assert_eq!(w[0].label, "session");
        assert_eq!(w[2].label, "weekly scoped (Fable)");
        assert!(matches!(w[2].severity, Severity::Warning));
        assert!(w[0].resets_at.is_some());
    }

    #[test]
    fn finds_top_level_windows() {
        let v: Value = serde_json::json!({
            "five_hour": {"utilization": 12.5, "resets_at": 1754179799},
            "seven_day": {"utilization": 95.0},
            "seven_day_opus": null,
            "extra_usage": {"utilization": 0.0}
        });
        let w = parse_usage(&v).windows;
        assert_eq!(w.len(), 3);
        assert_eq!(w[0].label, "five hour");
        assert_eq!(w[0].percent, 12.5);
        assert!(w[0].resets_at.is_some());
        assert!(matches!(w[2].severity, Severity::Exceeded));
    }

    #[test]
    fn keeps_everything_else_as_facts() {
        let v: Value = serde_json::json!({
            "organization": {"tier": "max", "seats": 3},
            "extra_usage": {"is_enabled": false, "spend": {"amount": 12.5}},
            "access_token": "sk-should-not-show",
            "limits": [{"kind": "session", "percent": 5}]
        });
        let facts = parse_usage(&v).facts;
        let labels: Vec<&str> = facts.iter().map(|f| f.label.as_str()).collect();
        assert_eq!(
            labels,
            ["organization · tier", "organization · seats", "extra usage · is enabled", "extra usage · spend · amount"]
        );
        assert_eq!(facts[1].value, "3");
        assert_eq!(facts[2].value, "no");
        assert_eq!(facts[3].value, "12.50");
    }

    #[test]
    fn unknown_shape_yields_empty_not_panic() {
        assert!(parse_usage(&serde_json::json!({"whatever": 3})).windows.is_empty());
        assert!(parse_usage(&serde_json::json!([1, 2])).windows.is_empty());
        assert!(parse_usage(&serde_json::json!([1, 2])).facts.is_empty());
    }
}
