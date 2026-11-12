//! Sessions running on the live login, read from the registry and transcripts claude code keeps

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use accio_provider::{contract, SessionState};
use serde::Deserialize;
use serde_json::Value;

const REREADS: usize = 4;
const REREAD_GAP: Duration = Duration::from_millis(25);

// One registry record whose process is still running
pub struct Record {
    pub now: SessionState,
    pub name: String,
    pub dir: String,
    pub session_id: Option<String>,
}

// The login a change moves the live sessions to
pub enum Target<'a> {
    Org { name: &'a str, org: String },
    Keyed { name: &'a str },
    Unknown,
}

// Every running session in a config directory's registry, oldest first, spares left out
pub fn records(home: &Path) -> Vec<Record> {
    let Ok(entries) = fs::read_dir(home.join("sessions")) else {
        return Vec::new();
    };
    let mut found: Vec<(u64, Record)> = entries
        .flatten()
        .filter_map(|e| record(&e.path()))
        .collect();
    found.sort_by_key(|(started, _)| *started);
    found.into_iter().map(|(_, record)| record).collect()
}

fn record(path: &Path) -> Option<(u64, Record)> {
    let stem = path.file_stem()?.to_str()?;
    let pid: u32 = stem.parse().ok()?;
    if path.extension()? != "json" || pid.to_string() != stem {
        return None;
    }
    let Some(v) = read_settled(path, pid) else {
        // A running process whose record stays unreadable is listed as busy so no hold lands on it
        let token = start_token(pid)?;
        return path.exists().then(|| {
            let now = SessionState {
                id: format!("{pid}:{token}"),
                state: "registry record unreadable".to_string(),
                mid_turn: true,
            };
            let record = Record {
                now,
                name: format!("pid {pid}"),
                dir: "?".to_string(),
                session_id: None,
            };
            (0, record)
        });
    };
    if v.get("spare").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    let text = |key: &str| v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty());
    let foreign = text("pidDomain").is_some_and(|d| d != own_domain());
    // Claude code also counts a record from another machine or pid namespace as running
    let token = if foreign {
        None
    } else {
        Some(start_token(pid)?)
    };
    let start = match (text("procStart"), token) {
        (Some(recorded), Some(token)) if recorded != token => return None,
        (Some(recorded), _) => recorded.to_string(),
        (None, token) => token.unwrap_or_default(),
    };
    let session_id = text("sessionId").map(str::to_string);
    let name = text("name")
        .map(str::to_string)
        .or_else(|| session_id.as_ref().map(|id| id.chars().take(8).collect()))
        .unwrap_or_else(|| format!("pid {pid}"));
    let (state, mid_turn) = state(text("status"), text("waitingFor"));
    let record = Record {
        now: SessionState {
            id: format!("{pid}:{start}"),
            state,
            mid_turn,
        },
        name,
        dir: text("cwd").map_or_else(|| "?".to_string(), |dir| contract(Path::new(dir))),
        session_id,
    };
    Some((
        v.get("startedAt").and_then(Value::as_u64).unwrap_or(0),
        record,
    ))
}

// A record caught half written is read again while its process lives
fn read_settled(path: &Path, pid: u32) -> Option<Value> {
    for attempt in 0..REREADS {
        if attempt > 0 {
            std::thread::sleep(REREAD_GAP);
        }
        let parsed = fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .filter(Value::is_object);
        if parsed.is_some() || start_token(pid).is_none() {
            return parsed;
        }
    }
    None
}

// Words for the status a session reports, anything but a finished turn counts as mid turn
fn state(status: Option<&str>, waiting_for: Option<&str>) -> (String, bool) {
    match (status, waiting_for) {
        (Some("idle"), _) => ("idle".to_string(), false),
        (Some("shell"), _) => ("idle, shell running".to_string(), false),
        (Some("busy"), _) => ("mid-turn".to_string(), true),
        (Some("waiting"), Some(what)) => (format!("needs you, {what}"), true),
        (Some("waiting"), None) => ("needs you".to_string(), true),
        (Some(other), _) => (other.to_string(), true),
        (None, _) => ("no state reported".to_string(), true),
    }
}

// The token claude code stores as procStart, none once the process is gone
#[cfg(target_os = "linux")]
fn start_token(pid: u32) -> Option<String> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat[stat.rfind(')')? + 1..]
        .split(' ')
        .nth(20)
        .map(str::to_string)
}

#[cfg(not(target_os = "linux"))]
fn start_token(pid: u32) -> Option<String> {
    let out = std::process::Command::new("ps")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .env("LC_ALL", "C")
        .env("TZ", "UTC")
        .output()
        .ok()?;
    let token = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !token.is_empty()).then_some(token)
}

// The pidDomain claude code writes on this machine
fn own_domain() -> &'static str {
    static DOMAIN: OnceLock<String> = OnceLock::new();
    DOMAIN.get_or_init(|| {
        if !cfg!(target_os = "linux") {
            return "linux".to_string();
        }
        let machine = fs::read_to_string("/etc/machine-id").unwrap_or_default();
        let space = fs::read_link("/proc/self/ns/pid")
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        format!("linux:{}:{space}", machine.trim())
    })
}

fn transcript(home: &Path, session_id: &str) -> Option<PathBuf> {
    let plain = session_id
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-');
    if session_id.is_empty() || !plain {
        return None;
    }
    fs::read_dir(home.join("projects"))
        .ok()?
        .flatten()
        .map(|e| e.path().join(format!("{session_id}.jsonl")))
        .find(|p| p.is_file())
}

#[derive(Deserialize)]
struct Head {
    #[serde(rename = "type")]
    kind: Option<String>,
}

struct Turn {
    parent: Option<String>,
    thinking: usize,
    org: Option<String>,
}

// Thinking blocks on the conversation's current branch, counted per organization that wrote them
pub fn thinking(home: &Path, session_id: &str) -> Vec<(Option<String>, usize)> {
    let Some(file) = transcript(home, session_id).and_then(|p| fs::File::open(p).ok()) else {
        return Vec::new();
    };
    let mut turns: HashMap<String, Turn> = HashMap::new();
    let (mut org, mut leaf) = (None, None);
    for line in BufReader::new(file).split(b'\n').map_while(Result::ok) {
        // Request logs dwarf the conversation, only its own entries get a full parse
        let Some(kind) = serde_json::from_slice::<Head>(&line)
            .ok()
            .and_then(|head| head.kind)
            .filter(|k| matches!(k.as_str(), "user" | "assistant" | "attachment" | "system"))
        else {
            continue;
        };
        let Ok(v) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        let Some(uuid) = v.get("uuid").and_then(Value::as_str) else {
            continue;
        };
        if v.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        if v.pointer("/attachment/type").and_then(Value::as_str) == Some("credential_org") {
            org = v
                .pointer("/attachment/organizationUuid")
                .and_then(Value::as_str)
                .map(str::to_lowercase);
        }
        let blocks = v
            .pointer("/message/content")
            .and_then(Value::as_array)
            .filter(|_| kind == "assistant");
        let thinking = blocks.map_or(0, |blocks| {
            blocks
                .iter()
                .filter_map(|b| b.get("type").and_then(Value::as_str))
                .filter(|t| matches!(*t, "thinking" | "redacted_thinking"))
                .count()
        });
        let turn = Turn {
            parent: v
                .get("parentUuid")
                .and_then(Value::as_str)
                .map(str::to_string),
            thinking,
            org: org.clone(),
        };
        turns.insert(uuid.to_string(), turn);
        leaf = Some(uuid.to_string());
    }
    // Only the chain behind the newest entry is still in the conversation
    let mut branch = Vec::new();
    while let Some(turn) = leaf.take().and_then(|id| turns.get(&id)) {
        if branch.len() >= turns.len() {
            break;
        }
        branch.push(turn);
        leaf = turn.parent.clone();
    }
    let mut tally: Vec<(Option<String>, usize)> = Vec::new();
    for turn in branch.iter().rev().filter(|t| t.thinking > 0) {
        match tally.iter_mut().find(|(org, _)| *org == turn.org) {
            Some((_, count)) => *count += turn.thinking,
            None => tally.push((turn.org.clone(), turn.thinking)),
        }
    }
    tally
}

// What the login change does to a conversation's thinking, one clause per writer
pub fn effect(
    tally: &[(Option<String>, usize)],
    target: &Target,
    writer: &dyn Fn(&str) -> Option<String>,
) -> String {
    if tally.is_empty() {
        return "no thinking in its conversation".to_string();
    }
    let clauses: Vec<String> = tally
        .iter()
        .map(|(org, count)| {
            let from = match org {
                Some(org) => {
                    writer(org).unwrap_or_else(|| "an account accio has not saved".to_string())
                }
                None => "a key or endpoint login".to_string(),
            };
            let blocks = if *count == 1 { "block" } else { "blocks" };
            let held = format!("{count} thinking {blocks} from {from}");
            match (target, org) {
                (Target::Org { org: to, .. }, Some(org)) if to == org => {
                    format!("{held} stay readable")
                }
                (Target::Org { name, .. }, _) | (Target::Keyed { name }, Some(_)) => {
                    format!("{held} unreadable as {name}")
                }
                (Target::Unknown, Some(_)) => {
                    format!("{held}, readable only under that organization")
                }
                (Target::Keyed { .. } | Target::Unknown, None) => held,
            }
        })
        .collect();
    clauses.join(" · ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::process::{Child, Command};

    fn home(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("accio-live-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(d.join("sessions")).unwrap();
        fs::create_dir_all(d.join("projects/-work")).unwrap();
        d
    }

    fn sleeper() -> Child {
        Command::new("sleep").arg("30").spawn().unwrap()
    }

    fn register(home: &Path, pid: u32, mut body: Value) {
        if body.get("procStart").is_none() {
            body["procStart"] = Value::from(start_token(pid).unwrap());
        }
        body["pid"] = Value::from(pid);
        fs::write(home.join(format!("sessions/{pid}.json")), body.to_string()).unwrap();
    }

    #[test]
    fn registry_lists_running_sessions_with_their_turn_state() {
        let h = home("registry");
        let mut kids: Vec<Child> = (0..7).map(|_| sleeper()).collect();
        let pid = |i: usize| kids[i].id();
        register(
            &h,
            pid(0),
            json!({"name": "busy-one", "status": "busy", "cwd": "/srv/app", "startedAt": 5,
                   "sessionId": "aaaaaaaa-1111"}),
        );
        register(
            &h,
            pid(1),
            json!({"status": "waiting", "waitingFor": "permission", "startedAt": 2,
                   "sessionId": "bbbbbbbb-2222"}),
        );
        register(
            &h,
            pid(2),
            json!({"name": "shelled", "status": "shell", "startedAt": 3}),
        );
        register(
            &h,
            pid(3),
            json!({"name": "resting", "status": "idle", "startedAt": 4}),
        );
        register(
            &h,
            pid(4),
            json!({"name": "spare", "status": "idle", "spare": true}),
        );
        register(
            &h,
            pid(5),
            json!({"name": "recycled", "status": "busy", "procStart": "1"}),
        );
        register(&h, pid(6), json!({"name": "quiet", "startedAt": 6}));
        // A pid no process holds, once under this machine and once under another
        let gone = 4_000_000_000u32;
        fs::write(
            h.join(format!("sessions/{gone}.json")),
            json!({"name": "dead", "status": "busy", "procStart": "9"}).to_string(),
        )
        .unwrap();
        fs::write(
            h.join(format!("sessions/{}.json", gone + 1)),
            json!({"name": "elsewhere", "status": "idle", "procStart": "9", "startedAt": 1,
                   "pidDomain": "linux:another-machine:pid:[1]"})
            .to_string(),
        )
        .unwrap();
        fs::write(h.join("sessions/007.json"), "{}").unwrap();
        fs::write(h.join("sessions/notes.json"), "{}").unwrap();
        fs::write(h.join(format!("sessions/{}.key", pid(0))), "secret").unwrap();

        let found = records(&h);
        let seen: Vec<(&str, &str, bool)> = found
            .iter()
            .map(|r| (r.name.as_str(), r.now.state.as_str(), r.now.mid_turn))
            .collect();
        assert_eq!(
            seen,
            [
                ("elsewhere", "idle", false),
                ("bbbbbbbb", "needs you, permission", true),
                ("shelled", "idle, shell running", false),
                ("resting", "idle", false),
                ("busy-one", "mid-turn", true),
                ("quiet", "no state reported", true),
            ]
        );
        assert_eq!(found[4].dir, "/srv/app");
        assert_eq!(found[4].session_id.as_deref(), Some("aaaaaaaa-1111"));
        assert_eq!(
            found[4].now.id,
            format!("{}:{}", pid(0), start_token(pid(0)).unwrap())
        );
        assert_eq!(found[0].now.id, format!("{}:9", gone + 1));

        // A record caught half written still holds its place, and a finished process leaves
        fs::write(
            h.join(format!("sessions/{}.json", pid(3))),
            "{\"name\": \"res",
        )
        .unwrap();
        let torn = records(&h);
        let resting = torn
            .iter()
            .find(|r| r.name == format!("pid {}", pid(3)))
            .expect("the live process stays listed");
        assert_eq!(resting.now.id, found[3].now.id);
        assert!(resting.now.mid_turn);
        assert_eq!(resting.now.state, "registry record unreadable");
        kids[0].kill().unwrap();
        kids[0].wait().unwrap();
        assert!(records(&h).iter().all(|r| r.name != "busy-one"));
        for kid in &mut kids[1..] {
            kid.kill().unwrap();
            kid.wait().unwrap();
        }
        assert_eq!(
            records(&h).len(),
            1,
            "only the other machine's record is left"
        );
        assert!(records(&h.join("nowhere")).is_empty());
    }

    #[test]
    fn backend_lists_live_sessions_with_what_each_target_does_to_their_thinking() {
        use accio_provider::Backend;
        use std::collections::BTreeMap;
        let h = home("backend");
        let mut kid = sleeper();
        register(
            &h,
            kid.id(),
            json!({"name": "api", "status": "busy", "cwd": "/srv/app", "sessionId": "sess-1"}),
        );
        let lines = [
            entry("attachment", "o1", None, org("org-a")),
            entry(
                "assistant",
                "a1",
                Some("o1"),
                says(&["thinking", "thinking"]),
            ),
        ];
        fs::write(h.join("projects/-work/sess-1.jsonl"), lines.join("\n")).unwrap();
        let claude = crate::Claude {
            home: h.clone(),
            live: crate::store::Slot::live(&h, true),
            claude_json_path: h.join(".claude.json"),
            settings_path: h.join("settings.json"),
            settings_key: h.join("settings.json").display().to_string(),
            stores: h.join("accounts"),
            sessions: h.join("private"),
        };
        let login = |org: &str| {
            let account = json!({"organizationUuid": org}).to_string();
            BTreeMap::from([("oauth_account".to_string(), account)])
        };
        let (alpha, beta) = (login("ORG-A"), login("org-b"));
        let nameless = BTreeMap::from([("oauth_account".to_string(), "{}".to_string())]);
        let glm = BTreeMap::from([("merge:x".to_string(), "{}".to_string())]);
        let profiles = [
            ("alpha", &alpha),
            ("beta", &beta),
            ("glm", &glm),
            ("nameless", &nameless),
        ];
        let effect = |target| claude.sessions(&profiles, target).remove(0).effect;
        assert_eq!(
            effect(Some(0)),
            "2 thinking blocks from alpha stay readable"
        );
        assert_eq!(
            effect(Some(1)),
            "2 thinking blocks from alpha unreadable as beta"
        );
        assert_eq!(
            effect(Some(2)),
            "2 thinking blocks from alpha unreadable as glm"
        );
        let elsewhere = "2 thinking blocks from alpha, readable only under that organization";
        assert_eq!(effect(Some(3)), elsewhere);
        assert_eq!(effect(None), elsewhere);

        let listed = claude.sessions(&profiles, Some(1));
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "api");
        assert_eq!(listed[0].dir, "/srv/app");
        assert_eq!(listed[0].now.state, "mid-turn");
        assert_eq!(claude.session_states(), [listed[0].now.clone()]);
        kid.kill().unwrap();
        kid.wait().unwrap();
        assert!(claude.session_states().is_empty());
        assert!(claude.sessions(&profiles, Some(1)).is_empty());
    }

    fn entry(kind: &str, uuid: &str, parent: Option<&str>, extra: Value) -> String {
        let mut v = json!({"type": kind, "uuid": uuid, "parentUuid": parent, "isSidechain": false});
        for (k, val) in extra.as_object().unwrap() {
            v[k] = val.clone();
        }
        v.to_string()
    }

    fn says(blocks: &[&str]) -> Value {
        let content: Vec<Value> = blocks.iter().map(|t| json!({"type": t})).collect();
        json!({"message": {"role": "assistant", "content": content}})
    }

    fn org(uuid: &str) -> Value {
        json!({"attachment": {"type": "credential_org", "organizationUuid": uuid}})
    }

    #[test]
    fn thinking_counts_follow_the_current_branch_and_the_login_that_wrote_them() {
        let h = home("thinking");
        let lines = [
            json!({"type": "mode", "mode": "normal"}).to_string(),
            entry("user", "u1", None, json!({"message": {"content": "hi"}})),
            entry("assistant", "a0", Some("u1"), says(&["thinking", "text"])),
            entry("attachment", "o1", Some("a0"), org("ORG-A")),
            entry(
                "assistant",
                "a1",
                Some("o1"),
                says(&["thinking", "redacted_thinking", "text"]),
            ),
            // A rewound branch and a subagent's lines stay out of the count
            entry(
                "assistant",
                "dead",
                Some("a1"),
                says(&["thinking", "thinking", "thinking"]),
            ),
            json!({"type": "assistant", "uuid": "side", "parentUuid": "a1", "isSidechain": true,
                   "message": {"content": [{"type": "thinking"}]}})
            .to_string(),
            json!({"type": "api-request-blob", "message": {"content": [{"type": "thinking"}]}})
                .to_string(),
            "{\"type\": \"assistant\", \"uuid\": \"torn".to_string(),
            entry(
                "user",
                "u2",
                Some("a1"),
                json!({"message": {"content": [{"type": "tool_result"}]}}),
            ),
            entry("attachment", "o2", Some("u2"), org("org-b")),
            entry("assistant", "a2", Some("o2"), says(&["thinking"])),
            entry("attachment", "o3", Some("a2"), org("org-a")),
            entry(
                "assistant",
                "a3",
                Some("o3"),
                says(&["thinking", "tool_use"]),
            ),
            entry(
                "system",
                "s1",
                Some("a3"),
                json!({"subtype": "turn_duration"}),
            ),
        ];
        fs::write(h.join("projects/-work/sess-1.jsonl"), lines.join("\n")).unwrap();
        assert_eq!(
            thinking(&h, "sess-1"),
            [
                (None, 1),
                (Some("org-a".to_string()), 3),
                (Some("org-b".to_string()), 1)
            ]
        );

        // A compaction starts a new chain, what came before it is no longer sent
        let compacted = [
            entry("assistant", "old", None, says(&["thinking", "thinking"])),
            entry("attachment", "o1", Some("old"), org("org-a")),
            entry(
                "system",
                "cut",
                None,
                json!({"subtype": "compact_boundary", "logicalParentUuid": "o1"}),
            ),
            entry("assistant", "new", Some("cut"), says(&["thinking"])),
        ];
        fs::write(h.join("projects/-work/sess-2.jsonl"), compacted.join("\n")).unwrap();
        assert_eq!(thinking(&h, "sess-2"), [(Some("org-a".to_string()), 1)]);

        let looped = [
            entry("assistant", "x", Some("y"), says(&["thinking"])),
            entry("assistant", "y", Some("x"), says(&["thinking"])),
        ];
        fs::write(h.join("projects/-work/sess-3.jsonl"), looped.join("\n")).unwrap();
        assert_eq!(thinking(&h, "sess-3"), [(None, 2)]);

        fs::write(h.join("projects/-work/sess-4.jsonl"), "").unwrap();
        assert!(thinking(&h, "sess-4").is_empty());
        assert!(thinking(&h, "sess-9").is_empty());
        assert!(thinking(&h, "../sessions/x").is_empty());
        assert!(thinking(&h, "").is_empty());
    }

    #[test]
    fn effect_names_each_writer_and_what_the_target_can_still_read() {
        let writer = |org: &str| (org == "org-a").then(|| "alpha".to_string());
        let tally = [
            (Some("org-a".to_string()), 204),
            (Some("org-b".to_string()), 1),
            (None, 3),
        ];
        let to_alpha = Target::Org {
            name: "alpha",
            org: "org-a".to_string(),
        };
        assert_eq!(
            effect(&tally, &to_alpha, &writer),
            "204 thinking blocks from alpha stay readable · \
             1 thinking block from an account accio has not saved unreadable as alpha · \
             3 thinking blocks from a key or endpoint login unreadable as alpha"
        );
        let to_beta = Target::Org {
            name: "beta",
            org: "org-c".to_string(),
        };
        assert_eq!(
            effect(&tally[..1], &to_beta, &writer),
            "204 thinking blocks from alpha unreadable as beta"
        );
        assert_eq!(
            effect(&tally, &Target::Keyed { name: "glm" }, &writer),
            "204 thinking blocks from alpha unreadable as glm · \
             1 thinking block from an account accio has not saved unreadable as glm · \
             3 thinking blocks from a key or endpoint login"
        );
        assert_eq!(
            effect(&tally, &Target::Unknown, &writer),
            "204 thinking blocks from alpha, readable only under that organization · \
             1 thinking block from an account accio has not saved, readable only under that \
             organization · 3 thinking blocks from a key or endpoint login"
        );
        assert_eq!(
            effect(&[], &to_alpha, &writer),
            "no thinking in its conversation"
        );
    }
}
