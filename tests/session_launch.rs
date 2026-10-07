use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

struct Fixture {
    root: PathBuf,
    config: PathBuf,
    profiles: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "accio-launch-test-{}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let config = if cfg!(target_os = "macos") {
            root.join("Library/Application Support")
        } else {
            root.join(".config")
        };
        let profiles = config.join("accio/accounts/claude");
        fs::create_dir_all(&profiles).unwrap();
        fs::create_dir_all(root.join(".claude")).unwrap();
        fs::create_dir(root.join("bin")).unwrap();
        fs::write(
            root.join(".claude/settings.json"),
            json!({
                "permissions": {"deny": ["Read(secret)"]},
                "apiKeyHelper": "wrong-helper",
                "env": {"ANTHROPIC_AUTH_TOKEN":"wrong-user-token", "EDITOR":"vim"}
            })
            .to_string(),
        )
        .unwrap();
        fs::write(root.join(".claude.json"), json!({
            "hasCompletedOnboarding":true, "oauthAccount":{"emailAddress":"original@example.com"},
            "primaryApiKey":"wrong-global-key"
        }).to_string()).unwrap();
        fs::write(
            root.join(".claude/.credentials.json"),
            "original credentials",
        )
        .unwrap();
        fs::write(profiles.join(".active"), "original").unwrap();
        fs::create_dir(root.join("shared-skills")).unwrap();
        fs::write(
            root.join("shared-skills/instructions.md"),
            "original instructions",
        )
        .unwrap();
        symlink(root.join("shared-skills"), root.join(".claude/skills")).unwrap();
        let fake = root.join("bin/claude");
        fs::write(
            &fake,
            r##"#!/bin/sh
set -eu
if [ "${1:-}" = "--version" ]; then printf '2.1.285 (Claude Code)\n'; exit 0; fi
test "$CLAUDE_CONFIG_DIR" = "$CLAUDE_SECURESTORAGE_CONFIG_DIR"
printf '%s\n' "$CLAUDE_CONFIG_DIR"
printf '%s\n' "$@" > "$CLAUDE_CONFIG_DIR/arguments"
printf '%s' "$ANTHROPIC_AUTH_TOKEN" > "$CLAUDE_CONFIG_DIR/child-token"
printf '%s' "$PWD" > "$CLAUDE_CONFIG_DIR/cwd"
printf '%s' 'session edit' > "$CLAUDE_CONFIG_DIR/skills/instructions.md"
sleep 0.05
exit "${FAKE_EXIT:-0}"
"##,
        )
        .unwrap();
        fs::set_permissions(fake, fs::Permissions::from_mode(0o755)).unwrap();
        Self {
            root,
            config,
            profiles,
        }
    }

    fn command(&self) -> Command {
        self.command_for("claude")
    }

    fn command_for(&self, provider: &str) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_accio"));
        command
            .arg(provider)
            .env("HOME", &self.root)
            .env("XDG_CONFIG_HOME", &self.config)
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.root.join("bin").display()),
            )
            .env("ANTHROPIC_AUTH_TOKEN", "wrong-shell-token")
            .env("CLAUDE_SECURESTORAGE_CONFIG_DIR", "wrong-inherited-store")
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("CODEX_HOME")
            .env_remove("GROK_HOME")
            .env_remove("GEMINI_CLI_HOME")
            .current_dir(&self.root);
        command
    }

    fn profile(&self, name: &str, oauth: bool) {
        let files = if oauth {
            json!({"credentials": json!({"claudeAiOauth": {
                "accessToken": format!("oauth-{name}"), "refreshToken":"fake-refresh", "expiresAt":4102444800000_i64
            }}).to_string(), "oauth_account":json!({"emailAddress": format!("{name}@example.com")}).to_string()})
        } else {
            json!({"merge:~/.claude/settings.json":json!({"env":{
                "ANTHROPIC_AUTH_TOKEN":format!("token-{name}"), "ANTHROPIC_BASE_URL":"https://example.invalid"
            }}).to_string()})
        };
        fs::write(
            self.profiles.join(format!("{name}.json")),
            json!({"files":files}).to_string(),
        )
        .unwrap();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn read_json(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

#[test]
fn twenty_concurrent_launches_keep_credentials_and_original_files_isolated() {
    let f = Fixture::new();
    for i in 0..20 {
        f.profile(&format!("account-{i}"), i % 2 == 0);
    }
    let originals: Vec<_> = [
        f.root.join(".claude/settings.json"),
        f.root.join(".claude/.credentials.json"),
        f.root.join(".claude.json"),
        f.root.join("shared-skills/instructions.md"),
        f.profiles.join(".active"),
    ]
    .into_iter()
    .chain((0..20).map(|i| f.profiles.join(format!("account-{i}.json"))))
    .map(|path| {
        let bytes = fs::read(&path).unwrap();
        (path, bytes)
    })
    .collect();
    let children: Vec<_> = (0..20)
        .map(|i| {
            f.command()
                .args([
                    "--profile",
                    &format!("account-{i}"),
                    "--",
                    "-p",
                    "a prompt with spaces",
                ])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    let mut dirs = BTreeSet::new();
    for (i, child) in children.into_iter().enumerate() {
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let dir = PathBuf::from(String::from_utf8(out.stdout).unwrap().trim());
        assert!(
            dirs.insert(dir.clone()),
            "sessions must have distinct directories"
        );
        assert_eq!(
            fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::read_to_string(dir.join("arguments")).unwrap(),
            format!(
                "--settings\n{}\n-p\na prompt with spaces\n",
                dir.join("accio-settings.json").display()
            )
        );
        assert_eq!(
            PathBuf::from(fs::read_to_string(dir.join("cwd")).unwrap())
                .canonicalize()
                .unwrap(),
            f.root.canonicalize().unwrap()
        );
        let settings = read_json(&dir.join("settings.json"));
        assert_eq!(settings["permissions"]["deny"][0], "Read(secret)");
        assert_eq!(settings["env"]["EDITOR"], "vim");
        assert!(settings["env"].get("ANTHROPIC_AUTH_TOKEN").is_none());
        assert!(settings.get("apiKeyHelper").is_none());
        let config = read_json(&dir.join(".claude.json"));
        assert!(config.get("primaryApiKey").is_none());
        let token = fs::read_to_string(dir.join("child-token")).unwrap();
        if i % 2 == 0 {
            assert!(token.is_empty());
            assert_eq!(
                read_json(&dir.join(".credentials.json"))["claudeAiOauth"]["accessToken"],
                format!("oauth-account-{i}")
            );
            assert_eq!(
                config["oauthAccount"]["emailAddress"],
                format!("account-{i}@example.com")
            );
        } else {
            assert_eq!(token, format!("token-account-{i}"));
            assert!(!dir.join(".credentials.json").exists());
            assert!(config.get("oauthAccount").is_none());
        }
        assert_eq!(
            fs::metadata(dir.join("accio-settings.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    for (path, bytes) in originals {
        assert_eq!(
            fs::read(&path).unwrap(),
            bytes,
            "{} changed",
            path.display()
        );
    }
}

#[test]
fn exit_status_errors_and_failed_launch_cleanup() {
    let f = Fixture::new();
    f.profile("work", false);
    let output = f
        .command()
        .args(["--profile", "work"])
        .env("FAKE_EXIT", "42")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(42));
    let sessions = f.config.join("accio/sessions/claude");
    assert_eq!(fs::read_dir(&sessions).unwrap().count(), 1);
    let output = f.command().args(["--profile", "missing"]).output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("no claude profile"));
    let output = f
        .command()
        .args(["--profile", "work"])
        .env("PATH", "/nonexistent")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("could not run `claude`"));
    assert_eq!(fs::read_dir(&sessions).unwrap().count(), 1);
}

#[test]
fn codex_grok_and_gemini_isolate_login_and_configured_profiles() {
    let f = Fixture::new();
    for provider in ["codex", "grok", "gemini"] {
        let live = f.root.join(format!(".{provider}"));
        fs::create_dir(&live).unwrap();
        let config_name = if provider == "gemini" {
            "settings.json"
        } else {
            "config.toml"
        };
        let config = if provider == "gemini" {
            "{\"ui\":{\"theme\":\"dark\"}}"
        } else {
            "# original user settings\n"
        };
        fs::write(live.join(config_name), config).unwrap();
        fs::write(live.join("auth.json"), "live credentials").unwrap();
        let profiles = f.config.join("accio/accounts").join(provider);
        fs::create_dir_all(&profiles).unwrap();
        let fake = f.root.join("bin").join(provider);
        fs::write(&fake, r##"#!/bin/sh
set -eu
if [ "${1:-}" = "--version" ]; then printf '99.0.0\n'; exit 0; fi
case "${0##*/}" in
codex) private_dir="$CODEX_HOME"; test -z "${OPENAI_API_KEY:-}"; test -z "${CODEX_API_KEY:-}" ;;
grok) private_dir="$GROK_HOME"; printf '%s' "${XAI_API_KEY:-}" > "$private_dir/api-key" ;;
gemini) private_dir="$GEMINI_CLI_HOME/.gemini"; test "$GEMINI_FORCE_FILE_STORAGE" = true; printf '%s' "$GEMINI_API_KEY" > "$private_dir/api-key" ;;
esac
printf '%s\n' "$private_dir"
printf '%s\n' "$@" > "$private_dir/arguments"
sleep 0.05
"##).unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
        let mut children = Vec::new();
        for name in ["login", "key"] {
            let files = match (provider, name) {
                ("gemini", "login") => {
                    json!({"~/.gemini/oauth_creds.json":"{\"refresh_token\":\"gemini-login\"}", "~/.gemini/google_accounts.json":"{\"active\":\"work@example.com\"}"})
                }
                ("gemini", _) => {
                    json!({"~/.gemini/.env":"GEMINI_API_KEY=gemini-key\nGOOGLE_GEMINI_BASE_URL=https://example.invalid\n"})
                }
                ("codex", "login") => {
                    json!({"~/.codex/auth.json":"{\"tokens\":{\"refresh_token\":\"codex-login\"}}"})
                }
                ("codex", _) => json!({"~/.codex/auth.json":"{\"OPENAI_API_KEY\":\"codex-key\"}"}),
                ("grok", "login") => {
                    json!({"~/.grok/auth.json":"{\"access_token\":\"grok-login\"}"})
                }
                _ => json!({"~/.grok/auth.json":"{\"GROK_API_KEY\":\"grok-key\"}"}),
            };
            fs::write(
                profiles.join(format!("{name}.json")),
                json!({"files":files}).to_string(),
            )
            .unwrap();
            children.push(
                f.command_for(provider)
                    .args(["--profile", name, "--", "prompt with spaces"])
                    .env("OPENAI_API_KEY", "wrong")
                    .env("CODEX_API_KEY", "wrong")
                    .env("XAI_API_KEY", "wrong")
                    .env("GEMINI_API_KEY", "wrong")
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .unwrap(),
            );
        }
        let mut dirs = BTreeSet::new();
        for (i, child) in children.into_iter().enumerate() {
            let out = child.wait_with_output().unwrap();
            assert!(
                out.status.success(),
                "{provider}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            let dir = PathBuf::from(String::from_utf8(out.stdout).unwrap().trim());
            assert!(dirs.insert(dir.clone()));
            let args = fs::read_to_string(dir.join("arguments")).unwrap();
            if provider == "codex" {
                assert_eq!(
                    args,
                    "-c\ncli_auth_credentials_store=\"file\"\nprompt with spaces\n"
                );
                let auth = read_json(&dir.join("auth.json"));
                assert_eq!(
                    if i == 0 {
                        &auth["tokens"]["refresh_token"]
                    } else {
                        &auth["OPENAI_API_KEY"]
                    },
                    if i == 0 { "codex-login" } else { "codex-key" }
                );
            } else {
                assert_eq!(args, "prompt with spaces\n");
                assert_eq!(
                    fs::read_to_string(dir.join("api-key")).unwrap(),
                    if i == 0 {
                        String::new()
                    } else {
                        format!("{provider}-key")
                    }
                );
            }
            if provider == "gemini" {
                let settings = read_json(&dir.join("settings.json"));
                assert_eq!(settings["ui"]["theme"], "dark");
                assert_eq!(
                    settings["security"]["auth"]["selectedType"],
                    if i == 0 {
                        "oauth-personal"
                    } else {
                        "gemini-api-key"
                    }
                );
                assert_eq!(dir.join("oauth_creds.json").exists(), i == 0);
            } else {
                assert_eq!(fs::read_to_string(dir.join(config_name)).unwrap(), config);
            }
            fs::write(dir.join(config_name), "session edit").unwrap();
        }
        assert_eq!(fs::read_to_string(live.join(config_name)).unwrap(), config);
        assert_eq!(
            fs::read_to_string(live.join("auth.json")).unwrap(),
            "live credentials"
        );
        assert!(!profiles.join(".active").exists());
    }
}

#[test]
fn old_harness_and_nonterminal_picker_fail_without_launching() {
    let f = Fixture::new();
    f.profile("work", false);
    let output = f.command().output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--profile NAME"));
    fs::write(f.root.join("bin/claude"), "#!/bin/sh\nprintf '2.0.0\\n'\n").unwrap();
    let output = f.command().args(["--profile", "work"]).output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("upgrade the harness"));
    assert_eq!(
        fs::read_dir(f.config.join("accio/sessions/claude"))
            .unwrap()
            .count(),
        0
    );
}
