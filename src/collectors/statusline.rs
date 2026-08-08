//! Claude Code statusline hook + spool ingestion.
//!
//! Claude Code persists NO utilization history anywhere; the statusline stdin
//! feed only exists while sessions run. merma therefore inserts itself as the
//! statusline command, appends every rate_limits snapshot to an append-only
//! spool, and chains to the user's real statusline command transparently.
//!
//! The hook path is latency-sensitive (runs on every statusline refresh) and
//! must never break the user's statusline: it does NOT touch SQLite (WAL lock
//! contention) and it logs its own failures to spool/hook-errors.log while
//! still chaining. `merma doctor` surfaces that log — quiet corruption is
//! avoided by never writing partial data (single O_APPEND write per line).

use super::ScanSummary;
use crate::config::{ensure_dir, Cfg};
use crate::store::{Snapshot, Store, CLAUDE};
use anyhow::{Context, Result};
use std::io::{Read, Write};
use std::process::{Command, Stdio};

/// Claude window ids from the statusline feed, with their (fixed, documented)
/// durations in minutes. Presence of each window is data — any may be absent.
pub const CLAUDE_WINDOWS: &[(&str, i64)] = &[
    ("five_hour", 300),
    ("seven_day", 10_080),
    ("seven_day_opus", 10_080),
    ("seven_day_sonnet", 10_080),
];

/// Entry point for `merma statusline-hook`. Reads the feed from stdin, spools
/// a snapshot, chains to the previous statusline command, prints its output.
pub fn run_hook(cfg: &Cfg) -> i32 {
    let mut input = Vec::new();
    if let Err(e) = std::io::stdin().read_to_end(&mut input) {
        log_hook_error(cfg, &format!("stdin read: {e}"));
    }
    if let Err(e) = spool_snapshot(cfg, &input) {
        log_hook_error(cfg, &format!("spool: {e:#}"));
    }
    match &cfg.file.chain_statusline {
        Some(cmd) if !cmd.trim().is_empty() => match chain(cmd, &input) {
            Ok(out) => {
                let mut stdout = std::io::stdout();
                let _ = stdout.write_all(&out);
                let _ = stdout.flush();
            }
            Err(e) => {
                log_hook_error(cfg, &format!("chain {cmd:?}: {e:#}"));
                println!("{}", fallback_line(&input));
            }
        },
        _ => println!("{}", fallback_line(&input)),
    }
    0
}

fn spool_snapshot(cfg: &Cfg, input: &[u8]) -> Result<()> {
    let v: serde_json::Value = serde_json::from_slice(input).context("statusline feed not JSON")?;
    let Some(rl) = v.get("rate_limits").filter(|r| r.is_object()) else {
        return Ok(()); // feed without rate limits (e.g. before first API response)
    };
    let line = serde_json::json!({
        "ts": chrono::Utc::now().timestamp(),
        "rate_limits": rl,
        "session_cost_usd": v.pointer("/cost/total_cost_usd"),
        "model": v.pointer("/model/id"),
    });
    ensure_dir(&cfg.spool_dir())?;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(cfg.statusline_spool())?;
    writeln!(f, "{line}")?;
    Ok(())
}

fn chain(cmd: &str, input: &[u8]) -> Result<Vec<u8>> {
    let mut child = Command::new("/bin/sh")
        .arg("-c")
        .arg(cmd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .context("spawning chained statusline")?;
    child
        .stdin
        .take()
        .context("no stdin pipe")?
        .write_all(input)
        .ok(); // chained command may not read stdin; that's fine
    let out = child
        .wait_with_output()
        .context("chained statusline failed")?;
    Ok(out.stdout)
}

fn fallback_line(input: &[u8]) -> String {
    let v: serde_json::Value = match serde_json::from_slice(input) {
        Ok(v) => v,
        Err(_) => return "merma".into(),
    };
    let mut parts = Vec::new();
    for (win, _) in CLAUDE_WINDOWS {
        if let Some(p) = v
            .pointer(&format!("/rate_limits/{win}/used_percentage"))
            .and_then(|x| x.as_f64())
        {
            let short = match *win {
                "five_hour" => "5h",
                "seven_day" => "7d",
                "seven_day_opus" => "7d-opus",
                "seven_day_sonnet" => "7d-sonnet",
                other => other,
            };
            parts.push(format!("{short} {p:.0}%"));
        }
    }
    if parts.is_empty() {
        "merma (no rate limits yet)".into()
    } else {
        parts.join(" · ")
    }
}

fn log_hook_error(cfg: &Cfg, msg: &str) {
    let _ = ensure_dir(&cfg.spool_dir());
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(cfg.hook_error_log())
    {
        let _ = writeln!(f, "{} {msg}", chrono::Utc::now().to_rfc3339());
    }
}

/// Ingest spooled statusline snapshots into the store (crash-safe rotation).
pub fn ingest_spool(store: &mut Store, cfg: &Cfg) -> Result<ScanSummary> {
    let mut sum = ScanSummary::default();
    let spool = cfg.statusline_spool();
    let ingesting = spool.with_extension("jsonl.ingesting");
    // Crash leftover first, then rotate the live spool.
    for stage in [&ingesting, &spool] {
        if stage == &spool && spool.exists() {
            std::fs::rename(&spool, &ingesting).context("rotating statusline spool")?;
        }
        if !ingesting.exists() {
            continue;
        }
        let text = std::fs::read_to_string(&ingesting)?;
        let mut snaps = Vec::new();
        for line in text.lines() {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                sum.parse_errors += 1;
                continue;
            };
            let Some(ts) = v.get("ts").and_then(|x| x.as_i64()) else {
                sum.parse_errors += 1;
                continue;
            };
            for (win, minutes) in CLAUDE_WINDOWS {
                let Some(w) = v
                    .pointer(&format!("/rate_limits/{win}"))
                    .filter(|w| w.is_object())
                else {
                    continue;
                };
                let pct = w
                    .get("used_percentage")
                    .or_else(|| w.get("utilization"))
                    .and_then(|x| x.as_f64());
                let Some(pct) = pct else { continue };
                if !pct.is_finite() || pct < 0.0 {
                    sum.parse_errors += 1;
                    continue;
                }
                snaps.push(Snapshot {
                    provider: CLAUDE.into(),
                    window_id: (*win).into(),
                    window_minutes: Some(*minutes),
                    used_percent: pct,
                    resets_at: w.get("resets_at").and_then(|x| x.as_i64()),
                    ts,
                    source: "statusline".into(),
                    plan_type: None,
                });
            }
        }
        sum.snapshots_inserted += store.insert_snapshots(&snaps)?;
        std::fs::remove_file(&ingesting).ok();
    }
    Ok(sum)
}
