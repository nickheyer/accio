use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::Utc;
use serde_json::{json, Value};

const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const TOKEN_URL: &str = "https://console.anthropic.com/v1/oauth/token";
const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(20))
        .build()
}

// Trades the refresh token for a new pair, the old refresh token is dead afterwards
pub fn refresh(creds: &mut Value) -> Result<()> {
    let oauth = creds
        .get("claudeAiOauth")
        .context("credentials have no claudeAiOauth section")?;
    let refresh_token = oauth
        .get("refreshToken")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .context("no refresh token - add this account again")?
        .to_string();
    let scopes: Vec<&str> = oauth
        .get("scopes")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let mut body = json!({
        "grant_type": "refresh_token",
        "refresh_token": refresh_token,
        "client_id": CLIENT_ID,
    });
    if !scopes.is_empty() {
        body["scope"] = Value::from(scopes.join(" "));
    }
    let resp: Value = agent()
        .post(TOKEN_URL)
        .set("Content-Type", "application/json")
        .send_json(body)
        .map_err(request_error)?
        .into_json()
        .context("token endpoint returned non-JSON")?;

    let access = match resp.get("access_token").and_then(Value::as_str) {
        Some(a) => a.to_string(),
        None => bail!("token refresh failed - log in to this account again"),
    };
    let now_ms = Utc::now().timestamp_millis();
    let oauth = creds.get_mut("claudeAiOauth").unwrap();
    oauth["accessToken"] = Value::from(access);
    if let Some(rt) = resp.get("refresh_token").and_then(Value::as_str) {
        oauth["refreshToken"] = Value::from(rt);
    }
    if let Some(exp) = resp.get("expires_in").and_then(Value::as_i64) {
        oauth["expiresAt"] = Value::from(now_ms + exp * 1000);
    }
    if let Some(exp) = resp.get("refresh_token_expires_in").and_then(Value::as_i64) {
        oauth["refreshTokenExpiresAt"] = Value::from(now_ms + exp * 1000);
    }
    if let Some(scope) = resp.get("scope").and_then(Value::as_str) {
        let granted: Vec<&str> = scope.split_whitespace().collect();
        if !granted.is_empty() {
            oauth["scopes"] = Value::from(granted);
        }
    }
    Ok(())
}

pub fn fetch_usage(access_token: &str) -> Result<Value> {
    agent()
        .get(USAGE_URL)
        .set("Authorization", &format!("Bearer {access_token}"))
        .set("anthropic-beta", "oauth-2025-04-20")
        .call()
        .map_err(request_error)?
        .into_json()
        .context("usage endpoint returned non-JSON")
}

fn request_error(e: ureq::Error) -> anyhow::Error {
    match e {
        ureq::Error::Status(code, resp) => {
            let body = resp.into_string().unwrap_or_default();
            let snippet: String = body.chars().take(120).collect();
            anyhow::anyhow!("HTTP {code}: {snippet}")
        }
        other => anyhow::anyhow!(other),
    }
}
