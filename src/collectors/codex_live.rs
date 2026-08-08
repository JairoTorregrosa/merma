//! Live Codex utilization via the wham usage endpoint.
//!
//! The Codex CLI itself polls `chatgpt.com/backend-api/wham/usage` (~60 s);
//! merma reuses the CLI's own auth.json tokens at a slower default cadence.
//! (The `codex app-server` JSON-RPC path exists too — `account/rateLimits/read`
//! — but proxies through a daemon; wham is the direct, verified path. If the
//! token has expired, any interactive codex run refreshes auth.json.)

use crate::config::Cfg;
use crate::store::{Snapshot, Store, CODEX};
use anyhow::{bail, Context, Result};
use std::time::Duration;

const WHAM_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";

pub struct WhamAuth {
    pub access_token: String,
    pub account_id: String,
}

pub fn read_auth(cfg: &Cfg) -> Result<WhamAuth> {
    let path = cfg.codex_auth_json();
    let text = std::fs::read_to_string(&path).with_context(|| {
        format!(
            "cannot read {} — remedy: run `codex login` once",
            path.display()
        )
    })?;
    let v: serde_json::Value = serde_json::from_str(&text).context("auth.json is not JSON")?;
    let tok = v
        .pointer("/tokens/access_token")
        .and_then(|x| x.as_str())
        .context("no tokens.access_token in auth.json — remedy: `codex login`")?;
    let acc = v
        .pointer("/tokens/account_id")
        .and_then(|x| x.as_str())
        .context("no tokens.account_id in auth.json — remedy: `codex login`")?;
    Ok(WhamAuth {
        access_token: tok.to_string(),
        account_id: acc.to_string(),
    })
}

/// One poll. Returns stored snapshots (primary/secondary as present).
pub fn poll(store: &mut Store, cfg: &Cfg) -> Result<Vec<Snapshot>> {
    let auth = read_auth(cfg)?;
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(20))
        .build();
    let resp = agent
        .get(WHAM_USAGE_URL)
        .set("Authorization", &format!("Bearer {}", auth.access_token))
        .set("ChatGPT-Account-Id", &auth.account_id)
        .set("User-Agent", "codex-cli")
        .call()
        .map_err(|e| match e {
            ureq::Error::Status(401, _) => anyhow::anyhow!(
                "wham/usage returned 401 — access token expired; remedy: run any interactive \
                 `codex` command to refresh ~/.codex/auth.json"
            ),
            ureq::Error::Status(code, _) => {
                anyhow::anyhow!("wham/usage returned HTTP {code} — endpoint may have changed")
            }
            other => anyhow::anyhow!("wham/usage unreachable: {other}"),
        })?;
    let v: serde_json::Value = resp.into_json().context("wham/usage response not JSON")?;
    let now = chrono::Utc::now().timestamp();
    let plan_type = v
        .get("plan_type")
        .and_then(|x| x.as_str())
        .map(str::to_string);
    let mut snaps = Vec::new();
    for (name, wid) in [
        ("primary_window", "primary"),
        ("secondary_window", "secondary"),
    ] {
        let Some(w) = v
            .pointer(&format!("/rate_limit/{name}"))
            .filter(|w| w.is_object())
        else {
            continue;
        };
        let Some(pct) = w.get("used_percent").and_then(|x| x.as_f64()) else {
            continue;
        };
        if !pct.is_finite() || pct < 0.0 {
            bail!("wham/usage returned invalid used_percent {pct} for {name}");
        }
        snaps.push(Snapshot {
            provider: CODEX.into(),
            window_id: wid.into(),
            window_minutes: w
                .get("limit_window_seconds")
                .and_then(|x| x.as_i64())
                .map(|s| s / 60)
                .filter(|m| *m > 0),
            used_percent: pct,
            resets_at: w.get("reset_at").and_then(|x| x.as_i64()),
            ts: now,
            source: "wham".into(),
            plan_type: plan_type.clone(),
        });
    }
    if snaps.is_empty() {
        bail!(
            "wham/usage returned no rate-limit windows — response keys: [{}]",
            v.as_object()
                .map(|o| o.keys().cloned().collect::<Vec<_>>().join(", "))
                .unwrap_or_default()
        );
    }
    store.insert_snapshots(&snaps)?;
    if let Some(rc) = v.get("rate_limit_reset_credits") {
        store.meta_set("codex_reset_credits", &rc.to_string())?;
    }
    Ok(snaps)
}
