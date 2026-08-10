//! Real oauth from cc, for getting usage stat
use anyhow::{bail, Context, Result};
use chrono::Utc;
use serde_json::{json, Value};

const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e"; // apparently claude code's client id
const TOKEN_URL: &str = "https://console.anthropic.com/v1/oauth/token";
const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";

// Try getting new token if expiry in 2 min or less, returns true if we got a new one
pub fn ensure_fresh(creds: &mut Value) -> Result<bool> {
    let oauth = creds
        .get("claudeAiOauth")
        .context("credentials have no claudeAiOauth section")?;
    let expires_at = oauth.get("expiresAt").and_then(Value::as_i64).unwrap_or(0);
    let now_ms = Utc::now().timestamp_millis();
    if expires_at - now_ms > 120_000 {
        return Ok(false);
    }
    let refresh_token = oauth
        .get("refreshToken")
        .and_then(Value::as_str)
        .context("no refresh token - re-add this account")?
        .to_string();

    let resp: Value = ureq::post(TOKEN_URL)
        .set("Content-Type", "application/json")
        .send_json(json!({
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
            "client_id": CLIENT_ID,
        }))
        .map_err(request_error)?
        .into_json()
        .context("token endpoint returned non-JSON")?;

    let access = match resp.get("access_token").and_then(Value::as_str) {
        Some(a) => a.to_string(),
        None => bail!("token refresh failed - account may need re-auth"),
    };
    let oauth = creds.get_mut("claudeAiOauth").unwrap();
    oauth["accessToken"] = Value::from(access);
    if let Some(rt) = resp.get("refresh_token").and_then(Value::as_str) {
        oauth["refreshToken"] = Value::from(rt);
    }
    if let Some(exp) = resp.get("expires_in").and_then(Value::as_i64) {
        oauth["expiresAt"] = Value::from(now_ms + exp * 1000);
    }
    Ok(true)
}

pub fn fetch_usage(access_token: &str) -> Result<Value> {
    ureq::get(USAGE_URL)
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
