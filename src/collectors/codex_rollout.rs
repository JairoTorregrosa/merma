//! Incremental scanner for Codex rollout JSONL files.
//!
//! Correctness invariants (verified against this machine's data, 2026-08):
//! - `total_token_usage` counters are CUMULATIVE per session → diff consecutive
//!   events; any negative component means a baseline reset (use current as delta).
//! - `sessions/` and `archived_sessions/` can hold the same rollout → dedup by
//!   file basename, active copy wins; dedup keys use the basename so a move
//!   never double-counts.
//! - `rate_limits` is null in `codex exec` rollouts → no snapshot, still usage.
//! - Window identity/count/duration is DATA (window_minutes, plan_type), never
//!   a constant: the 300-min primary regime flipped to 10080-min in Jul 2026.
//! - Model comes from the last `turn_context` before the event (session_meta as
//!   fallback for pre-turn_context eras); unknown stays "UNKNOWN" and is
//!   reported as unpriced, never silently priced.

use super::{iso_to_epoch, prefix_hash, resume_is_safe, ScanSummary};
use crate::config::Cfg;
use crate::store::{ScanState, Snapshot, Store, UsageEvent, CODEX};
use anyhow::Result;
use memchr::memmem;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Serialize, Deserialize)]
struct FileState {
    prev: Option<[i64; 4]>, // input, cached, cache_write, output (cumulative)
    model: Option<String>,
    #[serde(default)]
    prefix_hash: Option<u64>,
}

/// Snapshot source tag for one rollout file — carries provenance so a
/// rewritten rollout can retract exactly its own observations.
fn snapshot_source(basename: &str) -> String {
    format!("rollout:{basename}")
}

pub fn scan(store: &mut Store, cfg: &Cfg) -> Result<ScanSummary> {
    let mut sum = ScanSummary::default();
    // One-time provenance migration: snapshots written before per-file sources
    // (`source = "rollout"`) cannot be retracted when their file is rewritten.
    // Drop them and rescan every rollout from scratch — the files are the
    // ground truth and events dedup by key, so this is idempotent.
    let legacy: i64 = store.conn.query_row(
        "SELECT COUNT(*) FROM snapshots WHERE provider='codex' AND source='rollout'",
        [],
        |r| r.get(0),
    )?;
    if legacy > 0 {
        sum.warnings.push(format!(
            "migrating {legacy} legacy rollout snapshots to per-file provenance — full \
             codex rescan (one-time)"
        ));
        store.delete_snapshots_for_source("rollout")?;
        store.reset_scan_states(CODEX)?;
    }
    let mut by_basename: BTreeMap<String, PathBuf> = BTreeMap::new();
    for (i, root) in cfg.codex_session_roots().iter().enumerate() {
        if !root.is_dir() {
            if i == 0 {
                sum.warnings.push(format!(
                    "codex sessions dir missing: {} — no Codex history will be ingested",
                    root.display()
                ));
            }
            continue;
        }
        let is_archive = i == 1;
        for entry in walkdir::WalkDir::new(root) {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    sum.warnings.push(format!("rollout walk error: {e}"));
                    continue;
                }
            };
            let p = entry.path();
            if !p.is_file() {
                continue;
            }
            let name = p
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            if !name.starts_with("rollout-") || !name.ends_with(".jsonl") {
                continue;
            }
            // active copy (sessions/) wins over archived
            if is_archive && by_basename.contains_key(&name) {
                continue;
            }
            by_basename.insert(name, p.to_path_buf());
        }
    }
    sum.files_seen = by_basename.len();
    for (basename, path) in by_basename {
        match scan_file(store, &path, &basename) {
            Ok(s) => sum.absorb(s),
            Err(e) => sum.warnings.push(format!("{}: {e:#}", path.display())),
        }
    }
    Ok(sum)
}

fn scan_file(store: &mut Store, path: &Path, basename: &str) -> Result<ScanSummary> {
    let mut sum = ScanSummary::default();
    let meta = std::fs::metadata(path)?;
    let size = meta.len() as i64;
    let mtime = meta
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let key = path.display().to_string();
    let prior = store.scan_state(&key)?;
    // Full re-ingest: retract this file's events AND its snapshots (they carry
    // per-file provenance in `source`), then read from byte zero.
    let rewrite = |store: &Store, why: &str, sum: &mut ScanSummary| -> Result<ScanState> {
        sum.warnings
            .push(format!("{basename}: {why} — re-ingesting from scratch"));
        store.delete_events_for_file(basename)?;
        store.delete_snapshots_for_source(&snapshot_source(basename))?;
        Ok(ScanState::default())
    };
    let mut st = match prior {
        Some(p) if p.size == size && p.mtime == mtime => return Ok(sum), // unchanged
        Some(p) if p.size <= size => p,                                  // grew: resume candidate
        Some(_) => rewrite(store, "file shrank (rewritten)", &mut sum)?,
        None => ScanState::default(),
    };
    let mut fstate: FileState = match st.state_json.as_deref() {
        Some(js) => match serde_json::from_str(js) {
            Ok(f) => f,
            // Corrupt resume state must NEVER silently become an empty
            // baseline at a nonzero offset — that recounts cumulative totals.
            Err(e) => {
                st = rewrite(store, &format!("unreadable scan state ({e})"), &mut sum)?;
                FileState::default()
            }
        },
        None => FileState::default(),
    };
    if st.byte_offset > 0 && !resume_is_safe(path, fstate.prefix_hash, st.size, st.byte_offset)? {
        st = rewrite(store, "content changed under the resume offset", &mut sum)?;
        fstate = FileState::default();
    }

    let mut f = BufReader::new(std::fs::File::open(path)?);
    f.seek(SeekFrom::Start(st.byte_offset as u64))?;

    let tc = memmem::Finder::new(b"\"turn_context\"");
    let tok = memmem::Finder::new(b"\"token_count\"");
    let sm = memmem::Finder::new(b"\"session_meta\"");

    let mut events: Vec<UsageEvent> = Vec::new();
    let mut snaps: Vec<Snapshot> = Vec::new();
    let mut line = Vec::with_capacity(64 * 1024);
    loop {
        line.clear();
        let n = f.read_until(b'\n', &mut line)?;
        if n == 0 {
            break;
        }
        // Only ingest complete lines: a live session may have a partial tail.
        if line.last() != Some(&b'\n') {
            break;
        }
        st.byte_offset += n as i64;
        st.line_no += 1;
        let interesting =
            tc.find(&line).is_some() || tok.find(&line).is_some() || sm.find(&line).is_some();
        if !interesting {
            continue;
        }
        let v: serde_json::Value = match serde_json::from_slice(&line) {
            Ok(v) => v,
            Err(_) => {
                sum.parse_errors += 1;
                continue;
            }
        };
        let typ = v.get("type").and_then(|x| x.as_str()).unwrap_or("");
        let payload = v.get("payload");
        match typ {
            "turn_context" => {
                if let Some(m) = payload
                    .and_then(|p| p.get("model"))
                    .or_else(|| v.get("model"))
                    .and_then(|x| x.as_str())
                {
                    fstate.model = Some(m.to_string());
                }
            }
            "session_meta" => {
                if fstate.model.is_none() {
                    if let Some(m) = payload
                        .and_then(|p| p.get("model"))
                        .and_then(|x| x.as_str())
                    {
                        fstate.model = Some(m.to_string());
                    }
                }
            }
            "event_msg" => {
                let Some(p) = payload else { continue };
                if p.get("type").and_then(|x| x.as_str()) != Some("token_count") {
                    continue;
                }
                let ts = v
                    .get("timestamp")
                    .and_then(|x| x.as_str())
                    .and_then(iso_to_epoch);
                let Some(ts) = ts else {
                    sum.parse_errors += 1;
                    continue;
                };
                let plan_type = p
                    .pointer("/rate_limits/plan_type")
                    .and_then(|x| x.as_str())
                    .map(str::to_string);
                for wname in ["primary", "secondary"] {
                    let Some(w) = p
                        .pointer(&format!("/rate_limits/{wname}"))
                        .filter(|w| !w.is_null())
                    else {
                        continue;
                    };
                    let Some(pct) = w.get("used_percent").and_then(|x| x.as_f64()) else {
                        continue;
                    };
                    if !pct.is_finite() || pct < 0.0 {
                        sum.parse_errors += 1;
                        continue;
                    }
                    snaps.push(Snapshot {
                        provider: CODEX.into(),
                        window_id: wname.into(),
                        window_minutes: w
                            .get("window_minutes")
                            .and_then(|x| x.as_i64())
                            .filter(|m| *m > 0),
                        used_percent: pct,
                        resets_at: w.get("resets_at").and_then(|x| x.as_i64()),
                        ts,
                        source: snapshot_source(basename),
                        plan_type: plan_type.clone(),
                    });
                }
                let Some(tot) = p.pointer("/info/total_token_usage") else {
                    continue;
                };
                let g = |k: &str| tot.get(k).and_then(|x| x.as_i64()).unwrap_or(0);
                let cur = [
                    g("input_tokens"),
                    g("cached_input_tokens"),
                    g("cache_write_input_tokens"),
                    g("output_tokens"),
                ];
                // True token total: raw input already CONTAINS cached input,
                // so summing all four components would double-count cache hits.
                let true_total = |c: &[i64; 4]| c[0] + c[2] + c[3];
                let d = match fstate.prev {
                    Some(prev) => {
                        let d = [
                            cur[0] - prev[0],
                            cur[1] - prev[1],
                            cur[2] - prev[2],
                            cur[3] - prev[3],
                        ];
                        if d.iter().all(|&x| x >= 0) {
                            d
                        } else if d[0] < 0 {
                            // The dominant cumulative counter (raw input) went
                            // backwards: the baseline restarted — current IS
                            // the post-reset accumulation.
                            cur
                        } else {
                            // Input kept growing but a secondary component went
                            // backwards (e.g. only cache_write reset). Treating
                            // the whole vector as fresh would recount the
                            // entire session — clamp the anomaly and say so.
                            sum.warnings.push(format!(
                                "{basename}: cumulative counters moved inconsistently \
                                 (Δ={d:?}) — negative components clamped to 0"
                            ));
                            [d[0].max(0), d[1].max(0), d[2].max(0), d[3].max(0)]
                        }
                    }
                    // A brand-new file whose first counter already exceeds any
                    // plausible single turn (~2× the context window) is an
                    // inherited parent baseline (MultiAgent subagent replay,
                    // ccusage precedent) — baseline it, don't count it.
                    None if true_total(&cur) > 500_000 => {
                        sum.warnings.push(format!(
                            "{basename}: first token_count carries {} tokens — treated as \
                             inherited baseline, not usage",
                            true_total(&cur)
                        ));
                        [0, 0, 0, 0]
                    }
                    None => cur,
                };
                fstate.prev = Some(cur);
                if d.iter().all(|&x| x == 0) {
                    continue;
                }
                let model = fstate.model.clone().unwrap_or_else(|| "UNKNOWN".into());
                events.push(UsageEvent {
                    provider: CODEX,
                    ts,
                    model,
                    input: (d[0] - d[1]).max(0), // normalize: non-cached input
                    cached_input: d[1].max(0),
                    cache_w_5m: 0,
                    cache_w_1h: 0,
                    cache_w_unsplit: d[2].max(0),
                    output: d[3].max(0),
                    session_id: Some(basename.trim_end_matches(".jsonl").to_string()),
                    is_sidechain: false,
                    dedup_key: format!("{basename}:{}", st.line_no),
                    source_file: basename.to_string(),
                });
            }
            _ => {}
        }
    }
    sum.files_parsed = 1;
    sum.events_inserted = store.insert_usage_events(&events)?;
    sum.snapshots_inserted = store.insert_snapshots(&snaps)?;
    st.size = size;
    st.mtime = mtime;
    fstate.prefix_hash = Some(prefix_hash(path, size)?);
    st.state_json = Some(serde_json::to_string(&fstate)?);
    store.save_scan_state(&key, CODEX, &st)?;
    Ok(sum)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Cfg;

    fn cfg_with_codex_home(dir: &std::path::Path) -> Cfg {
        Cfg {
            merma_home: dir.join("merma"),
            claude_home: dir.join("claude"),
            codex_home: dir.to_path_buf(),
            file: Default::default(),
        }
    }

    /// Era A (2025-11): no limit_id, 300-min window. Era B (2026-08): weekly.
    /// Cumulative counters, a mid-session reset, and a null-rate_limits event.
    const ERA_A: &str = concat!(
        r#"{"timestamp":"2025-11-24T10:00:00.000Z","type":"session_meta","payload":{"cwd":"/x"}}"#,
        "\n",
        r#"{"timestamp":"2025-11-24T10:00:01.000Z","type":"turn_context","payload":{"model":"gpt-5.1-codex-max"}}"#,
        "\n",
        r#"{"timestamp":"2025-11-24T10:00:10.000Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1000,"cached_input_tokens":200,"output_tokens":50,"total_tokens":1050}},"rate_limits":{"primary":{"used_percent":1.0,"window_minutes":300,"resets_at":1764000000},"secondary":{"used_percent":3.0,"window_minutes":10080,"resets_at":1764500000},"credits":{"has_credits":false,"unlimited":false}}}}"#,
        "\n",
        r#"{"timestamp":"2025-11-24T10:05:00.000Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":3000,"cached_input_tokens":1200,"output_tokens":150,"total_tokens":3150}},"rate_limits":{"primary":{"used_percent":2.0,"window_minutes":300,"resets_at":1764000000},"secondary":{"used_percent":3.0,"window_minutes":10080,"resets_at":1764500000},"credits":{"has_credits":false,"unlimited":false}}}}"#,
        "\n",
    );

    const ERA_B: &str = concat!(
        r#"{"timestamp":"2026-08-07T01:00:00.000Z","type":"turn_context","payload":{"model":"gpt-5.6-sol"}}"#,
        "\n",
        r#"{"timestamp":"2026-08-07T01:00:10.000Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":16261,"cached_input_tokens":0,"cache_write_input_tokens":0,"output_tokens":131,"total_tokens":16392},"model_context_window":258400},"rate_limits":{"limit_id":"codex","limit_name":null,"primary":{"used_percent":18.0,"window_minutes":10080,"resets_at":1786176171},"secondary":null,"credits":{"has_credits":false,"unlimited":false,"balance":"0"},"plan_type":"plus"}}}"#,
        "\n",
        // cumulative RESET: counters go backwards → current becomes the delta
        r#"{"timestamp":"2026-08-07T01:10:00.000Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":500,"cached_input_tokens":100,"output_tokens":20,"total_tokens":520}},"rate_limits":null}}"#,
        "\n",
    );

    /// First token_count of a fresh file already at parent scale → baseline only.
    const INHERITED: &str = concat!(
        r#"{"timestamp":"2026-08-07T02:00:00.000Z","type":"turn_context","payload":{"model":"gpt-5.6-sol"}}"#,
        "\n",
        r#"{"timestamp":"2026-08-07T02:00:10.000Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":900000,"cached_input_tokens":800000,"output_tokens":5000,"total_tokens":905000}},"rate_limits":null}}"#,
        "\n",
        r#"{"timestamp":"2026-08-07T02:01:00.000Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":905000,"cached_input_tokens":803000,"output_tokens":5100,"total_tokens":910100}},"rate_limits":null}}"#,
        "\n",
    );

    fn write_rollout(root: &std::path::Path, rel: &str, content: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    #[test]
    fn parses_both_eras_and_dedups_archived() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = cfg_with_codex_home(tmp.path());
        write_rollout(
            tmp.path(),
            "sessions/2025/11/24/rollout-2025-11-24T10-00-00-aaa.jsonl",
            ERA_A,
        );
        write_rollout(
            tmp.path(),
            "sessions/2026/08/07/rollout-2026-08-07T01-00-00-bbb.jsonl",
            ERA_B,
        );
        // archived duplicate of the active file must NOT double-count
        write_rollout(
            tmp.path(),
            "archived_sessions/2025/11/24/rollout-2025-11-24T10-00-00-aaa.jsonl",
            ERA_A,
        );
        let mut store = crate::store::Store::open(&tmp.path().join("m.db")).unwrap();
        let sum = scan(&mut store, &cfg).unwrap();
        assert_eq!(sum.parse_errors, 0);
        let evs = store.usage_between(CODEX, 0, i64::MAX).unwrap();
        // era A: 2 events (first = baseline counts as usage in a fresh small file, second = diff)
        // era B: 2 events (16261 first, then reset-to-500)
        assert_eq!(evs.len(), 4);
        let a1 = &evs[0];
        assert_eq!(a1.model, "gpt-5.1-codex-max");
        assert_eq!(a1.input, 800); // 1000 - 200 cached
        assert_eq!(a1.cached_input, 200);
        let a2 = &evs[1];
        assert_eq!(a2.input, 1000); // Δinput 2000 - Δcached 1000
        assert_eq!(a2.cached_input, 1000);
        let b1 = &evs[2];
        assert_eq!(b1.model, "gpt-5.6-sol");
        assert_eq!(b1.input, 16261);
        let b2 = &evs[3];
        assert_eq!(b2.input, 400); // reset: current 500-100 non-cached
                                   // snapshots: era A produced primary+secondary per event, era B primary only
        let snaps = store.snapshots_between(CODEX, None, 0, i64::MAX).unwrap();
        assert_eq!(snaps.len(), 5);
        assert!(snaps
            .iter()
            .any(|s| s.window_id == "secondary" && s.window_minutes == Some(10080)));
        assert!(snaps.iter().any(|s| s.plan_type.as_deref() == Some("plus")));
    }

    #[test]
    fn inherited_baseline_not_counted() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = cfg_with_codex_home(tmp.path());
        write_rollout(
            tmp.path(),
            "sessions/2026/08/07/rollout-2026-08-07T02-00-00-ccc.jsonl",
            INHERITED,
        );
        let mut store = crate::store::Store::open(&tmp.path().join("m.db")).unwrap();
        let sum = scan(&mut store, &cfg).unwrap();
        assert!(sum
            .warnings
            .iter()
            .any(|w| w.contains("inherited baseline")));
        let evs = store.usage_between(CODEX, 0, i64::MAX).unwrap();
        // only the advancing delta after the baseline is usage
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].input, 2000); // Δinput 5000 - Δcached 3000
        assert_eq!(evs[0].output, 100);
    }

    /// Only one component going backwards while the total keeps growing is an
    /// anomaly to clamp, NOT a baseline reset — treating it as one would
    /// recount the whole cumulative history as fresh usage.
    #[test]
    fn partial_counter_regression_clamps_instead_of_recounting() {
        let content = concat!(
            r#"{"timestamp":"2026-08-07T03:00:00.000Z","type":"turn_context","payload":{"model":"gpt-5.6-sol"}}"#,
            "\n",
            r#"{"timestamp":"2026-08-07T03:00:10.000Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":10000,"cached_input_tokens":2000,"cache_write_input_tokens":5000,"output_tokens":500,"total_tokens":15500}},"rate_limits":null}}"#,
            "\n",
            // cache_write drops to 0 but input/output keep growing
            r#"{"timestamp":"2026-08-07T03:01:00.000Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":12000,"cached_input_tokens":2500,"cache_write_input_tokens":0,"output_tokens":600,"total_tokens":12600}},"rate_limits":null}}"#,
            "\n",
        );
        let tmp = tempfile::tempdir().unwrap();
        let cfg = cfg_with_codex_home(tmp.path());
        write_rollout(
            tmp.path(),
            "sessions/2026/08/07/rollout-2026-08-07T03-00-00-eee.jsonl",
            content,
        );
        let mut store = crate::store::Store::open(&tmp.path().join("m.db")).unwrap();
        let sum = scan(&mut store, &cfg).unwrap();
        assert!(sum.warnings.iter().any(|w| w.contains("inconsistently")));
        let evs = store.usage_between(CODEX, 0, i64::MAX).unwrap();
        assert_eq!(evs.len(), 2);
        let anomaly = &evs[1];
        // Δ = [2000, 500, -5000→0, 100]: growth kept, regression clamped —
        // and emphatically NOT the full 12k counter re-counted.
        assert_eq!(anomaly.input, 1500); // 2000 − 500 cached
        assert_eq!(anomaly.cache_w_unsplit, 0);
        assert_eq!(anomaly.output, 100);
    }

    #[test]
    fn incremental_resume_after_append() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = cfg_with_codex_home(tmp.path());
        let rel = "sessions/2026/08/07/rollout-2026-08-07T01-00-00-ddd.jsonl";
        write_rollout(tmp.path(), rel, ERA_B);
        let mut store = crate::store::Store::open(&tmp.path().join("m.db")).unwrap();
        scan(&mut store, &cfg).unwrap();
        // live session appends one more cumulative event
        let extra = r#"{"timestamp":"2026-08-07T01:20:00.000Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":700,"cached_input_tokens":150,"output_tokens":30,"total_tokens":730}},"rate_limits":null}}"#;
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(tmp.path().join(rel))
            .unwrap();
        writeln!(f, "{extra}").unwrap();
        // bump mtime so the scanner notices even on coarse filesystems
        let sum = scan(&mut store, &cfg).unwrap();
        assert_eq!(sum.parse_errors, 0);
        let evs = store.usage_between(CODEX, 0, i64::MAX).unwrap();
        assert_eq!(evs.len(), 3);
        let last = evs.last().unwrap();
        assert_eq!(last.input, 150); // Δinput 200 - Δcached 50 (from persisted state)
        assert_eq!(last.output, 10);
    }
}
