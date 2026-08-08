//! Unofficial Claude OAuth usage endpoint poller (opt-out via config).
//!
//! Terms-of-service note (honest disclosure, kept deliberately in-code):
//! Anthropic prohibits third-party use of subscription OAuth tokens (policy
//! tightened Feb 2026). This poller reads the token Claude Code itself stores
//! in the macOS Keychain and identifies with a claude-code User-Agent, at a
//! respectful cadence (default 180 s). The user opted in knowingly; set
//! `claude_oauth_enabled = false` in ~/.merma/config.toml to disable. The
//! statusline hook + transcripts provide the sanctioned alternative.

use crate::config::Cfg;
use crate::store::{Snapshot, Store, CLAUDE};
use anyhow::{bail, Context, Result};
use std::time::Duration;

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";

/// Windows the endpoint may return; each is nullable and appears/disappears by
/// account and era — presence is data, never assumed.
const OAUTH_WINDOWS: &[(&str, i64)] = &[
    ("five_hour", 300),
    ("seven_day", 10_080),
    ("seven_day_opus", 10_080),
    ("seven_day_sonnet", 10_080),
];

pub fn read_keychain_token() -> Result<String> {
    let out = std::process::Command::new("security")
        .args([
            "find-generic-password",
            "-s",
            "Claude Code-credentials",
            "-w",
        ])
        .output()
        .context("running `security` (macOS Keychain)")?;
    if !out.status.success() {
        bail!(
            "Keychain read failed ({}) — remedy: run any Claude Code session to refresh \
             credentials, or disable the OAuth poller (claude_oauth_enabled = false)",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).context("Keychain entry is not the expected JSON")?;
    let tok = v
        .pointer("/claudeAiOauth/accessToken")
        .and_then(|x| x.as_str())
        .context("no claudeAiOauth.accessToken in Keychain entry")?;
    Ok(tok.to_string())
}

/// One poll of the usage endpoint. Returns the snapshots it stored.
pub fn poll(store: &mut Store, cfg: &Cfg) -> Result<Vec<Snapshot>> {
    if cfg.file.claude_oauth_enabled == Some(false) {
        bail!("Claude OAuth poller disabled in config (claude_oauth_enabled = false)");
    }
    let token = read_keychain_token()?;
    let ua = format!("claude-code/{}", cfg.claude_version()?);
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(20))
        .build();
    let resp = agent
        .get(USAGE_URL)
        .set("Authorization", &format!("Bearer {token}"))
        .set("anthropic-beta", "oauth-2025-04-20")
        .set("User-Agent", &ua)
        .call()
        .map_err(|e| match e {
            ureq::Error::Status(code, _) => anyhow::anyhow!(
                "OAuth usage endpoint returned HTTP {code} — token may be expired (it rotates \
                 ~60 min; run a Claude Code session) or the endpoint changed"
            ),
            other => anyhow::anyhow!("OAuth usage endpoint unreachable: {other}"),
        })?;
    let v: serde_json::Value = resp.into_json().context("OAuth usage response not JSON")?;
    let now = chrono::Utc::now().timestamp();
    let mut snaps = Vec::new();
    for (win, minutes) in OAUTH_WINDOWS {
        let Some(w) = v.get(*win).filter(|w| w.is_object()) else {
            continue;
        };
        let Some(pct) = w.get("utilization").and_then(|x| x.as_f64()) else {
            continue;
        };
        if !pct.is_finite() || pct < 0.0 {
            bail!("oauth/usage returned invalid utilization {pct} for {win}");
        }
        let resets_at = w.get("resets_at").and_then(|x| {
            x.as_i64()
                .or_else(|| x.as_str().and_then(super::iso_to_epoch))
        });
        snaps.push(Snapshot {
            provider: CLAUDE.into(),
            window_id: (*win).into(),
            window_minutes: Some(*minutes),
            used_percent: pct,
            resets_at,
            ts: now,
            source: "oauth".into(),
            plan_type: None,
        });
    }
    if snaps.is_empty() {
        bail!(
            "OAuth usage endpoint returned no known windows — response keys: [{}]",
            v.as_object()
                .map(|o| o.keys().cloned().collect::<Vec<_>>().join(", "))
                .unwrap_or_default()
        );
    }
    store.insert_snapshots(&snaps)?;
    if let Some(extra) = v.get("extra_usage") {
        store.meta_set("claude_extra_usage", &extra.to_string())?;
    }
    Ok(snaps)
}
