//! Scan orchestration: incremental ingest of all local sources.

use crate::collectors::{claude_jsonl, codex_rollout, statusline, ScanSummary};
use crate::config::Cfg;
use crate::store::Store;
use anyhow::Result;

pub fn run_scan(store: &mut Store, cfg: &Cfg) -> Result<ScanSummary> {
    let mut sum = ScanSummary::default();
    sum.absorb(codex_rollout::scan(store, cfg)?);
    sum.absorb(claude_jsonl::scan(store, cfg)?);
    sum.absorb(statusline::ingest_spool(store, cfg)?);
    // Skipped lines never come back on later incremental scans — persist a
    // lifetime count so `merma doctor` can still surface historical loss.
    if sum.parse_errors > 0 {
        let prior: u64 = store
            .meta_get("parse_errors_total")?
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        store.meta_set(
            "parse_errors_total",
            &(prior + sum.parse_errors as u64).to_string(),
        )?;
    }
    Ok(sum)
}

/// Scan + live polls (for launchd / `merma collect`). Poll failures are
/// reported, never silently swallowed — but they don't abort the scan.
pub fn run_collect(store: &mut Store, cfg: &Cfg) -> Result<(ScanSummary, Vec<String>)> {
    let mut sum = run_scan(store, cfg)?;
    let mut poll_errors = Vec::new();
    if cfg.file.claude_oauth_enabled != Some(false) {
        if let Err(e) = crate::collectors::claude_oauth::poll(store, cfg) {
            poll_errors.push(format!("claude oauth poll: {e:#}"));
        } else {
            sum.snapshots_inserted += 1;
        }
    }
    match crate::collectors::codex_live::poll(store, cfg) {
        Ok(s) => sum.snapshots_inserted += s.len(),
        Err(e) => poll_errors.push(format!("codex wham poll: {e:#}")),
    }
    Ok((sum, poll_errors))
}
