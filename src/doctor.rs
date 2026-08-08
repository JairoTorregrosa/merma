//! `merma doctor` — source diagnostics and cross-checks.
//!
//! Every data source gets a ✓/⚠/✗ with a concrete remedy. Cross-checks compare
//! merma's latest stored snapshots against the live endpoints so the user can
//! also eyeball them against Claude Code `/usage` and
//! chatgpt.com/codex/settings/usage.

use crate::config::Cfg;
use crate::pricing::PriceBook;
use crate::store::{Store, CLAUDE, CODEX};
use anyhow::Result;
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct Check {
    pub name: String,
    pub status: String, // ok | warn | fail
    pub detail: String,
}

fn ok(name: &str, detail: String) -> Check {
    Check {
        name: name.into(),
        status: "ok".into(),
        detail,
    }
}
fn warn(name: &str, detail: String) -> Check {
    Check {
        name: name.into(),
        status: "warn".into(),
        detail,
    }
}
fn fail(name: &str, detail: String) -> Check {
    Check {
        name: name.into(),
        status: "fail".into(),
        detail,
    }
}

pub fn run(store: &mut Store, cfg: &Cfg, book: &PriceBook) -> Result<Vec<Check>> {
    let mut checks = Vec::new();

    // ── store ──
    let (events, snaps) = store.counts()?;
    checks.push(ok(
        "store",
        format!(
            "{} — {events} usage events, {snaps} snapshots",
            cfg.db_path().display()
        ),
    ));

    // ── price tables ──
    let (n_claude, n_openai) = book.entry_counts();
    checks.push(ok(
        "prices",
        format!(
            "{} — {n_claude} claude, {n_openai} openai entries",
            book.source
        ),
    ));

    // ── historical parse loss ──
    match store
        .meta_get("parse_errors_total")?
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0)
    {
        0 => {}
        n => checks.push(warn(
            "parse loss",
            format!(
                "{n} line(s) were unparseable across all scans — totals may undercount \
                 by that many requests"
            ),
        )),
    }

    // ── claude transcripts ──
    let projects = cfg.claude_projects_dir();
    if projects.is_dir() {
        let mut count = 0usize;
        let mut oldest: Option<i64> = None;
        for e in walkdir::WalkDir::new(&projects)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if e.path().extension().and_then(|x| x.to_str()) == Some("jsonl") {
                count += 1;
                if let Ok(m) = e.metadata() {
                    if let Ok(t) = m.modified() {
                        let ts = t
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs() as i64)
                            .unwrap_or(0);
                        oldest = Some(oldest.map_or(ts, |o: i64| o.min(ts)));
                    }
                }
            }
        }
        let age_days = oldest.map(|o| (chrono::Utc::now().timestamp() - o) / 86_400);
        checks.push(ok(
            "claude transcripts",
            format!(
                "{count} files, oldest ~{} days",
                age_days.map_or("?".into(), |d| d.to_string())
            ),
        ));
    } else {
        checks.push(fail(
            "claude transcripts",
            format!("{} missing", projects.display()),
        ));
    }

    // ── retention ──
    match std::fs::read_to_string(cfg.claude_settings_json())
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
    {
        Some(s) => {
            let hook = s
                .pointer("/statusLine/command")
                .and_then(|x| x.as_str())
                .unwrap_or("");
            if hook.contains("merma") {
                checks.push(ok("statusline hook", format!("installed ({hook})")));
            } else {
                checks.push(warn(
                    "statusline hook",
                    "NOT installed — Claude keeps no utilization history; run `merma install`"
                        .into(),
                ));
            }
            match s.get("cleanupPeriodDays").and_then(|x| x.as_i64()) {
                Some(d) if d >= 90 => checks.push(ok(
                    "transcript retention",
                    format!("cleanupPeriodDays = {d}"),
                )),
                other => checks.push(warn(
                    "transcript retention",
                    format!(
                        "cleanupPeriodDays = {} — transcripts evaporate; run \
                         `merma install --fix-retention`",
                        other.map_or("unset (default 30)".into(), |d| d.to_string())
                    ),
                )),
            }
        }
        None => checks.push(fail(
            "claude settings",
            format!("cannot read {}", cfg.claude_settings_json().display()),
        )),
    }

    // ── statusline spool ──
    let spool = cfg.statusline_spool();
    if spool.exists() {
        checks.push(ok("statusline spool", format!("{}", spool.display())));
    } else {
        checks.push(warn(
            "statusline spool",
            "empty (fills while Claude Code sessions run, after `merma install`)".into(),
        ));
    }
    if cfg.hook_error_log().exists() {
        let text = std::fs::read_to_string(cfg.hook_error_log()).unwrap_or_default();
        let n = text.lines().count();
        let last = text.lines().last().unwrap_or("").to_string();
        checks.push(warn("hook errors", format!("{n} logged; last: {last}")));
    }

    // ── claude plan ──
    match cfg.claude_plan(book) {
        Ok((label, usd, _)) => checks.push(ok("claude plan", format!("{label} (${usd}/mo)"))),
        Err(e) => checks.push(fail("claude plan", format!("{e:#}"))),
    }

    // ── claude oauth (unofficial, opt-out) ──
    if cfg.file.claude_oauth_enabled == Some(false) {
        checks.push(ok("claude oauth", "disabled in config".into()));
    } else {
        match crate::collectors::claude_oauth::poll(store, cfg) {
            Ok(snaps) => {
                let s: Vec<String> = snaps
                    .iter()
                    .map(|s| format!("{} {:.0}%", s.window_id, s.used_percent))
                    .collect();
                checks.push(ok("claude oauth (live)", s.join(" · ")));
            }
            Err(e) => checks.push(warn("claude oauth (live)", format!("{e:#}"))),
        }
    }

    // ── codex sessions ──
    let roots = cfg.codex_session_roots();
    if roots[0].is_dir() {
        checks.push(ok("codex sessions", format!("{}", roots[0].display())));
    } else {
        checks.push(fail(
            "codex sessions",
            format!("{} missing", roots[0].display()),
        ));
    }

    // ── codex live + cross-check ──
    match crate::collectors::codex_live::poll(store, cfg) {
        Ok(live) => {
            let s: Vec<String> = live
                .iter()
                .map(|s| format!("{} {:.0}%", s.window_id, s.used_percent))
                .collect();
            checks.push(ok("codex wham (live)", s.join(" · ")));
            // Cross-check: latest rollout snapshot vs live (same window).
            let now = chrono::Utc::now().timestamp();
            for l in &live {
                let recent =
                    store.snapshots_between(CODEX, Some(&l.window_id), now - 86_400, now)?;
                if let Some(r) = recent
                    .iter()
                    .rev()
                    .find(|r| r.source.starts_with("rollout"))
                {
                    let d = (r.used_percent - l.used_percent).abs();
                    let name = format!("cross-check codex {}", l.window_id);
                    if d <= 5.0 {
                        checks.push(ok(
                            &name,
                            format!(
                                "rollout {:.0}% vs live {:.0}% (Δ{d:.0} ≤ 5)",
                                r.used_percent, l.used_percent
                            ),
                        ));
                    } else {
                        checks.push(warn(
                            &name,
                            format!(
                                "rollout {:.0}% vs live {:.0}% (Δ{d:.0}) — rollouts may be stale",
                                r.used_percent, l.used_percent
                            ),
                        ));
                    }
                }
            }
        }
        Err(e) => checks.push(warn("codex wham (live)", format!("{e:#}"))),
    }

    // ── ingested coverage ──
    for p in [CODEX, CLAUDE] {
        match store.usage_bounds(p)? {
            Some((lo, hi)) => checks.push(ok(
                &format!("{p} usage history"),
                format!("{} → {}", crate::fmt::date(lo), crate::fmt::date(hi)),
            )),
            None => checks.push(warn(
                &format!("{p} usage history"),
                "no events ingested yet — run `merma scan`".into(),
            )),
        }
    }

    // ── calibration (the estimator's admission state, per provider) ──
    let now = chrono::Utc::now().timestamp();
    for p in [CODEX, CLAUDE] {
        let name = format!("{p} calibration");
        match crate::engine::waste::build_provider_report(
            store,
            book,
            cfg,
            p,
            now - 30 * 86_400,
            now,
        ) {
            Ok(r) => match &r.basis {
                Some(b) => {
                    let mut detail = format!(
                        "{} · {} qualifying {} instance(s)",
                        crate::brief::tier_name(b.tier),
                        b.n_qualifying,
                        b.window_id
                    );
                    if let Some(x) = crate::brief::excluded_phrase(&b.excluded) {
                        detail.push_str(&format!(" · {x}"));
                    }
                    if let Some(i) = &b.insufficient {
                        if let Some(ts) = i.unlocks_at {
                            detail.push_str(&format!(" · unlocks ~{}", crate::fmt::date(ts)));
                        }
                    }
                    match b.tier {
                        crate::engine::estimator::Tier::Insufficient => {
                            checks.push(warn(&name, detail))
                        }
                        _ => checks.push(ok(&name, detail)),
                    }
                }
                None => checks.push(warn(
                    &name,
                    "no window data yet — run `merma install` and use the windows".into(),
                )),
            },
            Err(e) => checks.push(warn(&name, format!("{e:#}"))),
        }
    }

    // ── used_percent quantization grid ──
    for p in [CODEX, CLAUDE] {
        let mut fractional: Option<f64> = None;
        for s in store.snapshots_between(p, None, 0, i64::MAX)? {
            if (s.used_percent - s.used_percent.round()).abs() >= 1e-6 {
                fractional = Some(s.used_percent);
                break;
            }
        }
        if let Some(pct) = fractional {
            checks.push(warn(
                &format!("{p} used_percent grid"),
                format!(
                    "non-integer used_percent observed ({pct}) — the quantization model \
                     may be wrong"
                ),
            ));
        }
    }

    // ── cross-window attribution consistency (diagnostic, never an adjustment) ──
    for p in [CODEX, CLAUDE] {
        match crate::engine::waste::attribution_cross_check(store, book, p, now) {
            Ok(Some(c)) if !c.mismatches.is_empty() => checks.push(warn(
                &format!("{p} cross-window attribution"),
                format!("attribution mismatch — {}", c.mismatches.join("; ")),
            )),
            Ok(Some(c)) if c.checked > 0 => checks.push(ok(
                &format!("{p} cross-window attribution"),
                format!(
                    "shorter-window attributions stay inside the long window \
                     ({} instance(s) checked)",
                    c.checked
                ),
            )),
            Ok(_) => {} // no concurrent shorter window to cross-check
            Err(e) => checks.push(warn(
                &format!("{p} cross-window attribution"),
                format!("{e:#}"),
            )),
        }
    }
    Ok(checks)
}

pub fn render_text(checks: &[Check]) -> String {
    use crate::theme::{dim, err_glyph, ok_glyph, warn_glyph};
    let mut out = String::new();
    for c in checks {
        let icon = match c.status.as_str() {
            "ok" => ok_glyph(),
            "warn" => warn_glyph(),
            _ => err_glyph(),
        };
        // Healthy plumbing recedes; problems are the full-brightness lines.
        let detail = if c.status == "ok" {
            dim(&c.detail)
        } else {
            c.detail.clone()
        };
        // The label column fits the longest check name ("claude cross-window
        // attribution", 31 cells) padded to 32 so, with the literal space in
        // the format string, every detail keeps a ≥ 2-space gutter.
        out.push_str(&format!("{icon} {:<32} {detail}\n", c.name));
    }
    out
}
